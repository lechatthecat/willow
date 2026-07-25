//! The atomic task state machine (willow-ezs.4).
//!
//! This is the contract the scheduler lock decomposition is built on, specified
//! and tested here before any queue is moved out from under the global mutex.
//!
//! One atomic word per task holds the lifecycle state plus three independent
//! flag bits. Every transition is a compare-exchange over the whole word, so a
//! thread that loses a race re-reads and re-decides rather than assuming.
//!
//! ```text
//! bits 0..2   lifecycle state (Ready/Running/Parked/BlockedSyscall/Cancelling/Terminal)
//! bit  3      queued           — exactly one run queue holds this task id
//! bit  4      wake_requested   — a wake arrived while the task was Running
//! bit  5      cancel_requested — cancellation asked for, consumed at a boundary
//! ```
//!
//! Invariants (I1–I6):
//!
//! - **I1** only a successful CAS `Ready -> Running` grants the right to poll;
//! - **I2** `queued` means exactly one runnable claim is outstanding: the id is
//!   either in a run queue or has been popped by the worker that is about to
//!   claim it. Pop and `Ready -> Running/Cancelling` consume that right in one
//!   CAS, so no `Ready + unqueued` window exists between them;
//! - **I3** a wake during `Running` sets `wake_requested` instead of enqueuing;
//! - **I4** `cancel_requested` is a request from any state, consumed at a
//!   scheduling boundary; it never preempts a running poll;
//! - **I5** exactly one thread transitions a task into `Terminal`, and only that
//!   thread publishes the result and detaches relationships;
//! - **I6** the frame is owned by whoever holds `Running` or `Cancelling`, and
//!   only that owner may suspend or finish the task.
//!
//! Two rules keep a task from being stranded, and they are the reason the
//! suspend and cancel paths are shaped the way they are:
//!
//! - a suspend (`park_after_poll`, `block_on_syscall`) resolves the poll result,
//!   a pending wake, and a pending cancel in *one* compare-exchange, and
//!   requeues instead of suspending if either flag is set;
//! - a cancel of a suspended task returns it to `Ready` and hands the caller the
//!   queue push, because nothing else would ever schedule it again.
//!
//! Neither is a nicety: skipping either one leaves a task parked forever with a
//! wake or a cancel it will never observe.
//!
//! Claims are `AcqRel`/`Acquire`, parks and terminal publication are `Release`,
//! and wakes are `AcqRel`. The word is an *additional* happens-before channel;
//! the frame status Release/Acquire pair documented in
//! `willow_happens_before.md` is unchanged and still carries the result itself.

use std::sync::atomic::{AtomicU32, Ordering};

/// Lifecycle state of a task, as stored in the low bits of the atomic word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TaskLifecycle {
    /// Runnable. Must be in exactly one run queue (I2).
    Ready = 0,
    /// A worker owns the frame and is polling it (I1, I6).
    Running = 1,
    /// Waiting on a timer, channel, or task completion.
    Parked = 2,
    /// Detached to the blocking pool, which owns the native syscall.
    BlockedSyscall = 3,
    /// Cancel-requested; a worker is running the frame's pending defers (I6).
    Cancelling = 4,
    /// Completed, panicked, or cancelled. Final; the result is published (I5).
    Terminal = 5,
}

impl TaskLifecycle {
    fn from_bits(bits: u32) -> Self {
        match bits {
            0 => Self::Ready,
            1 => Self::Running,
            2 => Self::Parked,
            3 => Self::BlockedSyscall,
            4 => Self::Cancelling,
            5 => Self::Terminal,
            other => unreachable!("invalid task lifecycle bits: {other}"),
        }
    }

    /// Whether a task in this state is finished for the purposes of an awaiter.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }

    /// Whether some thread currently owns the frame (I6).
    pub fn owns_frame(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling)
    }
}

const STATE_MASK: u32 = 0b111;
const QUEUED_BIT: u32 = 1 << 3;
const WAKE_REQUESTED_BIT: u32 = 1 << 4;
const CANCEL_REQUESTED_BIT: u32 = 1 << 5;

/// A decoded snapshot of the atomic word. Cheap to copy; never stale-checked in
/// place — callers re-read through the owning [`AtomicTaskState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskStateSnapshot {
    word: u32,
}

impl TaskStateSnapshot {
    pub fn lifecycle(self) -> TaskLifecycle {
        TaskLifecycle::from_bits(self.word & STATE_MASK)
    }

    /// I2: exactly one run queue holds this task id.
    pub fn is_queued(self) -> bool {
        self.word & QUEUED_BIT != 0
    }

    /// I3: a wake arrived while the task was running.
    pub fn wake_requested(self) -> bool {
        self.word & WAKE_REQUESTED_BIT != 0
    }

    /// I4: cancellation has been asked for but not yet consumed.
    pub fn cancel_requested(self) -> bool {
        self.word & CANCEL_REQUESTED_BIT != 0
    }
}

/// The outcome of a wake attempt, so the caller knows whether it now owns the
/// obligation to push the task onto a run queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    /// The task moved to `Ready` and this caller claimed the `queued` bit: the
    /// caller MUST push the id onto exactly one run queue.
    Enqueue,
    /// The task moved to `Ready` but another thread already owns the queue
    /// slot, or the task was already ready and queued. Nothing to do.
    AlreadyQueued,
    /// The task was `Running`, so `wake_requested` was set instead (I3). The
    /// polling worker will requeue it when the poll returns.
    DeferredToRunningPoll,
    /// The task is terminal; a wake is a no-op.
    Terminal,
}

/// The outcome of a suspend attempt at a poll boundary (park, or detach to the
/// blocking pool), so the caller knows whether the task actually suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryOutcome {
    /// The task suspended. It is in no run queue, and only a wake or a cancel
    /// request will return it to one.
    Suspended,
    /// A wake (I3) or a cancel request (I4) landed during the poll, so the task
    /// did NOT suspend: it is `Ready` and this caller claimed the queue slot.
    /// The caller MUST push the id onto exactly one run queue. Suspending here
    /// instead would strand the task: nothing else is left to wake it.
    Requeue,
    /// The caller did not own the frame — the state was neither `Running` nor
    /// `Cancelling` (I6) — so nothing was transitioned and there is nothing to
    /// push. Reaching this is a caller bug, but refusing the transition is
    /// safer than corrupting the state of whoever does own the frame.
    NotOwner,
}

/// The outcome of a cancellation request, so the caller knows whether it now
/// owns the obligation to push the task onto a run queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The request was recorded and the task was moved from a suspended state
    /// back to `Ready`; this caller claimed the queue slot and MUST push the id.
    /// Without this the cancel might not be observed promptly: a suspended task
    /// has no guaranteed near-term reason to be scheduled again.
    Enqueue,
    /// The request was recorded, and some other thread already owes the push:
    /// the frame owner will consume it at its next boundary, or the id is
    /// already in a run queue. Nothing to do.
    Deferred,
    /// Already cancel-requested, or terminal. Nothing was changed.
    NoChange,
}

/// The outcome of a claim attempt by a worker that popped this id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller owns the frame and must poll it (I1, I6).
    Poll,
    /// The task was cancel-requested; this caller owns the frame and must run
    /// its cleanup entry, then finish it terminally (I4).
    Cancel,
    /// The id was stale: another worker owns it, or it is already terminal.
    /// Drop the id.
    Drop,
}

/// The lifecycle word of one task.
#[derive(Debug)]
pub struct AtomicTaskState {
    word: AtomicU32,
}

impl Clone for AtomicTaskState {
    fn clone(&self) -> Self {
        Self {
            word: AtomicU32::new(self.word.load(Ordering::Acquire)),
        }
    }
}

impl Default for AtomicTaskState {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicTaskState {
    /// A freshly spawned task: ready, not yet in a queue. The spawner claims the
    /// queue slot with [`AtomicTaskState::claim_queue_slot`].
    pub fn new() -> Self {
        Self {
            word: AtomicU32::new(TaskLifecycle::Ready as u32),
        }
    }

    pub fn load(&self) -> TaskStateSnapshot {
        TaskStateSnapshot {
            word: self.word.load(Ordering::Acquire),
        }
    }

    pub fn lifecycle(&self) -> TaskLifecycle {
        self.load().lifecycle()
    }

    fn compare_exchange(&self, current: u32, new: u32, success: Ordering) -> Result<u32, u32> {
        self.word
            .compare_exchange_weak(current, new, success, Ordering::Acquire)
    }

    /// Claim the right to push this id onto a run queue (I2). Returns whether
    /// the caller won the slot; the winner MUST push exactly once, and the
    /// loser MUST NOT push.
    pub fn claim_queue_slot(&self) -> bool {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            if word & QUEUED_BIT != 0 {
                return false;
            }
            if TaskLifecycle::from_bits(word & STATE_MASK) != TaskLifecycle::Ready {
                return false;
            }
            match self.compare_exchange(word, word | QUEUED_BIT, Ordering::AcqRel) {
                Ok(_) => return true,
                Err(seen) => word = seen,
            }
        }
    }

    /// Atomically consume a popped queue entry and attempt to take ownership of
    /// the frame (I1/I2).
    ///
    /// The caller MUST NOT clear `queued` before this call. Keeping the bit set
    /// across the physical pop tells concurrent wakers that this worker already
    /// owns the obligation to poll. Clearing it first creates a
    /// `Ready + unqueued` window in which a waker can publish a second entry
    /// that this worker then accidentally consumes.
    ///
    /// A stale entry is consumed by clearing its queue slot in the same CAS and
    /// returns [`ClaimOutcome::Drop`].
    pub fn claim_for_poll(&self) -> ClaimOutcome {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            if word & QUEUED_BIT == 0 {
                return ClaimOutcome::Drop;
            }
            if TaskLifecycle::from_bits(word & STATE_MASK) != TaskLifecycle::Ready {
                let next = word & !QUEUED_BIT;
                match self.compare_exchange(word, next, Ordering::AcqRel) {
                    Ok(_) => return ClaimOutcome::Drop,
                    Err(seen) => word = seen,
                }
                continue;
            }
            // I4: cancellation is consumed here, at the scheduling boundary,
            // never by interrupting a poll already in flight. The request bit is
            // cleared as part of the same transition and the `Cancelling` state
            // takes over as the record of it — leaving the bit set would make
            // every later boundary re-decide to cancel a task that is already
            // cancelling, and a cancelling task could never park.
            let cancelling = word & CANCEL_REQUESTED_BIT != 0;
            let next_state = if cancelling {
                TaskLifecycle::Cancelling as u32
            } else {
                TaskLifecycle::Running as u32
            };
            // The claim also consumes the queue slot and any wake that arrived
            // while the task sat in the queue: this poll will observe it.
            let mut next = word & !STATE_MASK & !QUEUED_BIT & !WAKE_REQUESTED_BIT;
            if cancelling {
                next &= !CANCEL_REQUESTED_BIT;
            }
            let next = next | next_state;
            match self.compare_exchange(word, next, Ordering::AcqRel) {
                Ok(_) if cancelling => return ClaimOutcome::Cancel,
                Ok(_) => return ClaimOutcome::Poll,
                Err(seen) => word = seen,
            }
        }
    }

    /// Park after a poll returned Pending. See [`BoundaryOutcome`]: a wake or a
    /// cancel request that landed during the poll turns this into a requeue,
    /// because parking would leave nobody to schedule the task again.
    pub fn park_after_poll(&self) -> BoundaryOutcome {
        self.suspend_after_poll(TaskLifecycle::Parked)
    }

    /// Detach a running task to the blocking pool. Same contract as
    /// [`AtomicTaskState::park_after_poll`]: a wake or a cancel that landed
    /// during the poll wins, and the task stays runnable.
    pub fn block_on_syscall(&self) -> BoundaryOutcome {
        self.suspend_after_poll(TaskLifecycle::BlockedSyscall)
    }

    /// The one place a frame owner gives up the frame without finishing.
    ///
    /// Both suspends have to resolve the same three-way race in a single CAS:
    /// the poll finished, a wake may have landed (I3), and a cancel may have
    /// been requested (I4). Handling them in separate steps would let a wake or
    /// a cancel slip in between and be suspended away, stranding the task in
    /// `Parked`/`BlockedSyscall` with nothing left to schedule it.
    fn suspend_after_poll(&self, target: TaskLifecycle) -> BoundaryOutcome {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            let lifecycle = TaskLifecycle::from_bits(word & STATE_MASK);
            // I6: refuse rather than assert. The CAS source state is the real
            // guard; a caller that does not own the frame changes nothing.
            if !lifecycle.owns_frame() {
                return BoundaryOutcome::NotOwner;
            }
            let woken = word & WAKE_REQUESTED_BIT != 0;
            // A cancel that landed while the frame was `Running` must reach a
            // claim boundary so its cleanup starts; it cannot be suspended away.
            // A task that is already `Cancelling` has consumed its request, so
            // it may suspend — and re-arms the bit, so that when it is woken
            // again the next claim resumes cleanup instead of resuming the body.
            let cancel_pending =
                word & CANCEL_REQUESTED_BIT != 0 && lifecycle == TaskLifecycle::Running;
            let mut next = word & !STATE_MASK & !WAKE_REQUESTED_BIT;
            if lifecycle == TaskLifecycle::Cancelling {
                next |= CANCEL_REQUESTED_BIT;
            }
            if woken || cancel_pending {
                debug_assert_eq!(
                    word & QUEUED_BIT,
                    0,
                    "a task that owns its frame is never in a run queue (I2)"
                );
                // Take the queue slot as part of the same transition so no other
                // thread can also decide to enqueue this id.
                next |= QUEUED_BIT | TaskLifecycle::Ready as u32;
                match self.compare_exchange(word, next, Ordering::AcqRel) {
                    Ok(_) => return BoundaryOutcome::Requeue,
                    Err(seen) => word = seen,
                }
                continue;
            }
            next |= target as u32;
            match self.compare_exchange(word, next, Ordering::Release) {
                Ok(_) => return BoundaryOutcome::Suspended,
                Err(seen) => word = seen,
            }
        }
    }

    /// Requeue a task voluntarily (`await yield()`), keeping it runnable.
    /// [`BoundaryOutcome::Requeue`] means the caller claimed the queue slot and
    /// must push the id.
    pub fn requeue_after_poll(&self) -> BoundaryOutcome {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            let lifecycle = TaskLifecycle::from_bits(word & STATE_MASK);
            if !lifecycle.owns_frame() {
                return BoundaryOutcome::NotOwner;
            }
            debug_assert_eq!(
                word & QUEUED_BIT,
                0,
                "a task that owns its frame is never in a run queue (I2)"
            );
            let mut next = word & !STATE_MASK & !WAKE_REQUESTED_BIT;
            // As in `suspend_after_poll`: a cancelling task re-arms its request
            // so the next claim resumes cleanup.
            if lifecycle == TaskLifecycle::Cancelling {
                next |= CANCEL_REQUESTED_BIT;
            }
            let next = next | QUEUED_BIT | TaskLifecycle::Ready as u32;
            match self.compare_exchange(word, next, Ordering::AcqRel) {
                Ok(_) => return BoundaryOutcome::Requeue,
                Err(seen) => word = seen,
            }
        }
    }

    /// Wake a task from any state. See [`WakeOutcome`] for the caller's
    /// obligation.
    pub fn wake(&self) -> WakeOutcome {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            let lifecycle = TaskLifecycle::from_bits(word & STATE_MASK);
            match lifecycle {
                TaskLifecycle::Terminal => return WakeOutcome::Terminal,
                // I3: never enqueue a task another worker is polling.
                TaskLifecycle::Running | TaskLifecycle::Cancelling => {
                    let next = word | WAKE_REQUESTED_BIT;
                    match self.compare_exchange(word, next, Ordering::AcqRel) {
                        Ok(_) => return WakeOutcome::DeferredToRunningPoll,
                        Err(seen) => word = seen,
                    }
                }
                TaskLifecycle::Ready | TaskLifecycle::Parked | TaskLifecycle::BlockedSyscall => {
                    let already_queued = word & QUEUED_BIT != 0;
                    let next = (word & !STATE_MASK) | QUEUED_BIT | TaskLifecycle::Ready as u32;
                    match self.compare_exchange(word, next, Ordering::AcqRel) {
                        Ok(_) if already_queued => return WakeOutcome::AlreadyQueued,
                        Ok(_) => return WakeOutcome::Enqueue,
                        Err(seen) => word = seen,
                    }
                }
            }
        }
    }

    /// Request cancellation from any state (I4). See [`CancelOutcome`] for the
    /// caller's obligation.
    ///
    /// Recording the flag is not enough on its own: a `Parked` or
    /// `BlockedSyscall` task has nothing that is guaranteed to schedule it
    /// promptly, so the request also has to return it to `Ready` and hand the
    /// caller the queue push. The blocking-pool job may continue independently;
    /// its eventual completion wake is stale and safely ignored.
    pub fn request_cancel(&self) -> CancelOutcome {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            let lifecycle = TaskLifecycle::from_bits(word & STATE_MASK);
            if lifecycle.is_terminal() || word & CANCEL_REQUESTED_BIT != 0 {
                return CancelOutcome::NoChange;
            }
            let already_queued = word & QUEUED_BIT != 0;
            // A frame owner consumes the flag at its next boundary. Suspended
            // tasks are made runnable immediately so cancellation does not
            // depend on the event they were waiting for.
            let deferred = lifecycle.owns_frame();
            let mut next = word | CANCEL_REQUESTED_BIT;
            if !deferred {
                next = (next & !STATE_MASK) | QUEUED_BIT | TaskLifecycle::Ready as u32;
            }
            let ordering = if deferred {
                Ordering::Release
            } else {
                Ordering::AcqRel
            };
            match self.compare_exchange(word, next, ordering) {
                Ok(_) if deferred || already_queued => return CancelOutcome::Deferred,
                Ok(_) => return CancelOutcome::Enqueue,
                Err(seen) => word = seen,
            }
        }
    }

    /// Transition into `Terminal` (I5). Returns whether THIS caller performed
    /// the transition; only that caller may publish the result, wake waiters,
    /// and detach relationships.
    ///
    /// Only the frame owner may finish a task (I6), so the CAS accepts
    /// `Running` and `Cancelling` and nothing else. A task that must be torn
    /// down from a suspended state goes through [`AtomicTaskState::request_cancel`]
    /// and is finished by the worker that claims it — that is the only way the
    /// frame's pending defers are guaranteed to run exactly once, on a thread
    /// that actually owns the frame.
    pub fn finish_terminal(&self) -> bool {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            if !TaskLifecycle::from_bits(word & STATE_MASK).owns_frame() {
                return false;
            }
            // The queue slot is deliberately preserved: a duplicate id may still
            // be sitting in a run queue and its claimer will drop it.
            let next = (word & !STATE_MASK & !WAKE_REQUESTED_BIT & !CANCEL_REQUESTED_BIT)
                | TaskLifecycle::Terminal as u32;
            match self.compare_exchange(word, next, Ordering::Release) {
                Ok(_) => return true,
                Err(seen) => word = seen,
            }
        }
    }
}

/// Test perspectives for the atomic task state machine (S1–S35).
///
/// ```text
/// S1  a fresh task is Ready, unqueued, unflagged
/// S2  claiming the queue slot succeeds exactly once
/// S3  claim_for_poll consumes the queue slot in its ownership CAS
/// S4  a terminal task refuses a queue claim
/// S5  claim_for_poll on Ready takes the frame and consumes the queue slot
/// S6  claim_for_poll on a non-Ready task drops the stale id
/// S7  only one of many concurrent claimers wins (I1)
/// S8  claim_for_poll consumes a wake that arrived while queued
/// S9  a cancel-requested Ready task claims into Cancelling, not Running (I4)
/// S10 park_after_poll parks when nothing arrived during the poll
/// S11 park_after_poll refuses to park after a wake and claims the slot (I3)
/// S12 a wake during Running does not enqueue (I3)
/// S13 a wake on Parked enqueues exactly once
/// S14 a wake on an already-queued Ready task does not double-enqueue (I2)
/// S15 a wake on BlockedSyscall returns it to Ready
/// S16 a wake on a Terminal task is a no-op (I5)
/// S17 concurrent wakes on one parked task produce exactly one Enqueue (I2)
/// S18 cancellation can be requested from any non-terminal state (I4)
/// S19 a duplicate cancel request reports no change
/// S20 cancelling a terminal task reports no change
/// S21 exactly one of many concurrent finishers wins (I5)
/// S22 a barrier-pinned wake between physical pop and claim cannot publish a
///     second queue entry or be consumed as stale (I2)
/// S23 requeue_after_poll reports the slot only when it claims it
/// S24 block_on_syscall then wake round-trips to Ready
/// S25 cancelling a Parked task returns it to Ready and hands over the push
/// S26 cancelling a Running task defers, and that poll's boundary requeues it
///     instead of parking it with an unobserved cancel
/// S27 cancelling a BlockedSyscall task returns it to Ready immediately
/// S28 block_on_syscall refuses to block when a wake arrived during the poll
/// S29 block_on_syscall refuses to block when a cancel arrived during the poll
/// S30 a suspend, requeue, or terminal transition by a thread that does not own
///     the frame is refused and changes nothing (I6)
/// S31 a Cancelling task that suspends re-arms its request and resumes cleanup
///     on its next claim rather than resuming the task body
/// S32 claim_for_poll consumes the cancel request, so cleanup is entered once
/// S33 concurrent cancel vs park never leaves a task suspended with a pending
///     cancel, and produces exactly one queue push
/// S34 concurrent wake vs block_on_syscall never loses the wake, and produces
///     exactly one queue push
/// S35 concurrent cancel vs blocking completion produces exactly one queue push
/// ```
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Barrier};

    /// A task whose frame a worker owns. This is the only state a task may be
    /// suspended or finished from (I6), so most fixtures start here.
    fn running() -> AtomicTaskState {
        let state = AtomicTaskState::new();
        assert!(state.claim_queue_slot());
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Poll);
        state
    }

    fn parked() -> AtomicTaskState {
        let state = running();
        assert_eq!(state.park_after_poll(), BoundaryOutcome::Suspended);
        state
    }

    fn blocked() -> AtomicTaskState {
        let state = running();
        assert_eq!(state.block_on_syscall(), BoundaryOutcome::Suspended);
        state
    }

    fn terminal() -> AtomicTaskState {
        let state = running();
        assert!(state.finish_terminal());
        state
    }

    #[test]
    fn s01_fresh_task_is_ready_and_unqueued() {
        let state = AtomicTaskState::new();
        let snapshot = state.load();
        assert_eq!(snapshot.lifecycle(), TaskLifecycle::Ready);
        assert!(!snapshot.is_queued());
        assert!(!snapshot.wake_requested());
        assert!(!snapshot.cancel_requested());
    }

    #[test]
    fn s02_s03_queue_slot_is_claimed_once_and_consumed_by_poll_claim() {
        let state = AtomicTaskState::new();
        assert!(state.claim_queue_slot(), "S2: first claim wins");
        assert!(!state.claim_queue_slot(), "S2: second claim loses");
        assert!(state.load().is_queued());

        assert_eq!(state.claim_for_poll(), ClaimOutcome::Poll);
        assert!(!state.load().is_queued(), "S3: the poll claim consumes it");
        assert!(
            !state.claim_queue_slot(),
            "S3: a Running task cannot acquire another queue slot"
        );
    }

    #[test]
    fn s04_terminal_task_refuses_a_queue_claim() {
        let state = terminal();
        assert!(!state.claim_queue_slot(), "S4: terminal never re-enqueues");
    }

    #[test]
    fn s05_claim_for_poll_takes_the_frame_and_the_queue_slot() {
        let state = AtomicTaskState::new();
        state.claim_queue_slot();
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Poll);
        let snapshot = state.load();
        assert_eq!(snapshot.lifecycle(), TaskLifecycle::Running);
        assert!(snapshot.lifecycle().owns_frame(), "I6");
        assert!(!snapshot.is_queued(), "S5: the claim consumes the slot");
    }

    #[test]
    fn s06_claim_for_poll_drops_a_stale_id() {
        let state = parked();
        assert_eq!(
            state.claim_for_poll(),
            ClaimOutcome::Drop,
            "S6: a parked task's stale queue entry is dropped"
        );
        assert_eq!(terminal().claim_for_poll(), ClaimOutcome::Drop);
    }

    #[test]
    fn s07_only_one_concurrent_claimer_polls() {
        for _ in 0..200 {
            let state = Arc::new(AtomicTaskState::new());
            assert!(state.claim_queue_slot());
            let polls = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();
            for _ in 0..8 {
                let state = Arc::clone(&state);
                let polls = Arc::clone(&polls);
                handles.push(std::thread::spawn(move || {
                    if state.claim_for_poll() == ClaimOutcome::Poll {
                        polls.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
            assert_eq!(polls.load(Ordering::Relaxed), 1, "I1: exactly one claimer");
        }
    }

    #[test]
    fn s08_claim_consumes_a_wake_that_arrived_while_queued() {
        let state = AtomicTaskState::new();
        state.claim_queue_slot();
        assert_eq!(state.wake(), WakeOutcome::AlreadyQueued);
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Poll);
        assert!(
            !state.load().wake_requested(),
            "S8: the poll observes the wake, so the flag is cleared"
        );
    }

    #[test]
    fn s09_cancel_requested_ready_task_claims_into_cancelling() {
        let state = AtomicTaskState::new();
        assert_eq!(state.request_cancel(), CancelOutcome::Enqueue, "I4");
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel, "I4");
        assert_eq!(state.lifecycle(), TaskLifecycle::Cancelling);
        assert!(state.lifecycle().owns_frame(), "I6");
    }

    #[test]
    fn s10_park_after_poll_parks_when_nothing_arrived() {
        let state = running();
        assert_eq!(state.park_after_poll(), BoundaryOutcome::Suspended, "S10");
        assert_eq!(state.lifecycle(), TaskLifecycle::Parked);
        assert!(!state.load().is_queued());
    }

    #[test]
    fn s11_park_after_poll_requeues_when_a_wake_arrived() {
        let state = running();
        assert_eq!(state.wake(), WakeOutcome::DeferredToRunningPoll);
        assert_eq!(
            state.park_after_poll(),
            BoundaryOutcome::Requeue,
            "I3: must requeue, not park"
        );
        let snapshot = state.load();
        assert_eq!(snapshot.lifecycle(), TaskLifecycle::Ready);
        assert!(snapshot.is_queued(), "I3: the requeue claims the slot");
        assert!(!snapshot.wake_requested(), "the wake was consumed");
    }

    #[test]
    fn s12_a_wake_during_running_never_enqueues() {
        let state = running();
        assert_eq!(state.wake(), WakeOutcome::DeferredToRunningPoll);
        assert!(
            !state.load().is_queued(),
            "I3: a Running task must not be in a run queue"
        );
        assert!(state.load().wake_requested());
    }

    #[test]
    fn s13_a_wake_on_parked_enqueues_once() {
        let state = parked();
        assert_eq!(state.wake(), WakeOutcome::Enqueue);
        assert_eq!(state.lifecycle(), TaskLifecycle::Ready);
        assert!(state.load().is_queued());
    }

    #[test]
    fn s14_a_wake_on_an_already_queued_task_does_not_double_enqueue() {
        let state = AtomicTaskState::new();
        state.claim_queue_slot();
        assert_eq!(state.wake(), WakeOutcome::AlreadyQueued, "I2");
        assert!(state.load().is_queued());
    }

    #[test]
    fn s15_a_wake_on_blocked_syscall_returns_to_ready() {
        let state = blocked();
        assert_eq!(state.lifecycle(), TaskLifecycle::BlockedSyscall);
        assert_eq!(state.wake(), WakeOutcome::Enqueue);
        assert_eq!(state.lifecycle(), TaskLifecycle::Ready);
    }

    #[test]
    fn s16_a_wake_on_a_terminal_task_is_a_no_op() {
        let state = terminal();
        assert_eq!(state.wake(), WakeOutcome::Terminal, "I5");
        assert_eq!(state.lifecycle(), TaskLifecycle::Terminal);
    }

    #[test]
    fn s17_concurrent_wakes_enqueue_exactly_once() {
        for _ in 0..200 {
            let state = Arc::new(parked());
            let enqueues = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();
            for _ in 0..8 {
                let state = Arc::clone(&state);
                let enqueues = Arc::clone(&enqueues);
                handles.push(std::thread::spawn(move || {
                    if state.wake() == WakeOutcome::Enqueue {
                        enqueues.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
            assert_eq!(
                enqueues.load(Ordering::Relaxed),
                1,
                "I2: exactly one waker owns the queue push"
            );
        }
    }

    #[test]
    fn s18_cancellation_can_be_requested_from_any_live_state() {
        let ready = AtomicTaskState::new();
        assert_eq!(ready.request_cancel(), CancelOutcome::Enqueue);

        let running = running();
        assert_eq!(running.request_cancel(), CancelOutcome::Deferred);
        assert_eq!(
            running.lifecycle(),
            TaskLifecycle::Running,
            "I4: a request never preempts a running poll"
        );

        let parked = parked();
        assert_eq!(parked.request_cancel(), CancelOutcome::Enqueue);

        let blocked = blocked();
        assert_eq!(blocked.request_cancel(), CancelOutcome::Enqueue);
    }

    #[test]
    fn s19_s20_duplicate_and_terminal_cancel_requests_change_nothing() {
        let state = AtomicTaskState::new();
        assert_eq!(state.request_cancel(), CancelOutcome::Enqueue);
        assert_eq!(state.request_cancel(), CancelOutcome::NoChange, "S19");

        assert_eq!(terminal().request_cancel(), CancelOutcome::NoChange, "S20");
    }

    #[test]
    fn s21_exactly_one_finisher_publishes_the_terminal_transition() {
        for _ in 0..200 {
            let state = Arc::new(running());
            let finishers = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();
            for _ in 0..8 {
                let state = Arc::clone(&state);
                let finishers = Arc::clone(&finishers);
                handles.push(std::thread::spawn(move || {
                    if state.finish_terminal() {
                        finishers.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
            assert_eq!(
                finishers.load(Ordering::Relaxed),
                1,
                "I5: terminal publication happens exactly once"
            );
        }
    }

    #[test]
    fn s22_wake_between_physical_pop_and_claim_cannot_publish_a_duplicate() {
        for _ in 0..200 {
            let state = Arc::new(AtomicTaskState::new());
            assert!(state.claim_queue_slot());

            // The worker has physically popped the id, but deliberately leaves
            // `queued` set until claim_for_poll. Pin a wake in that interval.
            let popped = Arc::new(Barrier::new(2));
            let wake_done = Arc::new(Barrier::new(2));
            let waker = {
                let state = Arc::clone(&state);
                let popped = Arc::clone(&popped);
                let wake_done = Arc::clone(&wake_done);
                std::thread::spawn(move || {
                    popped.wait();
                    let outcome = state.wake();
                    wake_done.wait();
                    outcome
                })
            };

            popped.wait();
            wake_done.wait();
            assert_eq!(
                state.claim_for_poll(),
                ClaimOutcome::Poll,
                "the popped entry and ownership are consumed together"
            );
            assert_eq!(
                waker.join().unwrap(),
                WakeOutcome::AlreadyQueued,
                "S22: the in-flight pop remains the one scheduling obligation"
            );
            let snapshot = state.load();
            assert_eq!(snapshot.lifecycle(), TaskLifecycle::Running);
            assert!(!snapshot.is_queued());
            assert!(!snapshot.wake_requested());
        }
    }

    #[test]
    fn s23_requeue_claims_the_slot_and_is_refused_from_a_non_owner() {
        let state = running();
        assert_eq!(
            state.requeue_after_poll(),
            BoundaryOutcome::Requeue,
            "S23: claimed the slot"
        );
        assert!(state.load().is_queued());
        assert_eq!(
            state.requeue_after_poll(),
            BoundaryOutcome::NotOwner,
            "S23: the task is Ready again, so nobody may requeue it twice"
        );
    }

    #[test]
    fn s24_blocked_syscall_round_trips_to_ready() {
        let state = blocked();
        assert_eq!(state.lifecycle(), TaskLifecycle::BlockedSyscall);
        assert_eq!(state.wake(), WakeOutcome::Enqueue);
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Poll);
        assert_eq!(state.lifecycle(), TaskLifecycle::Running);
    }

    #[test]
    fn s25_cancelling_a_parked_task_makes_it_runnable_again() {
        let state = parked();
        assert_eq!(
            state.request_cancel(),
            CancelOutcome::Enqueue,
            "S25: nothing else would ever schedule this task again"
        );
        let snapshot = state.load();
        assert_eq!(snapshot.lifecycle(), TaskLifecycle::Ready);
        assert!(snapshot.is_queued(), "the canceller owns the push");
        assert!(snapshot.cancel_requested());
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);
    }

    #[test]
    fn s26_cancelling_a_running_task_requeues_it_at_its_poll_boundary() {
        let state = running();
        assert_eq!(state.request_cancel(), CancelOutcome::Deferred);
        assert_eq!(
            state.park_after_poll(),
            BoundaryOutcome::Requeue,
            "S26: parking here would strand the cancel forever"
        );
        let snapshot = state.load();
        assert_eq!(snapshot.lifecycle(), TaskLifecycle::Ready);
        assert!(snapshot.is_queued());
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);
    }

    #[test]
    fn s27_cancelling_a_blocked_task_makes_it_runnable_immediately() {
        let state = blocked();
        assert_eq!(
            state.request_cancel(),
            CancelOutcome::Enqueue,
            "S27: cancellation must not wait for the syscall to complete"
        );
        assert_eq!(state.lifecycle(), TaskLifecycle::Ready);
        assert!(state.load().is_queued());
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);
    }

    #[test]
    fn s28_block_on_syscall_refuses_to_block_after_a_wake() {
        let state = running();
        assert_eq!(state.wake(), WakeOutcome::DeferredToRunningPoll);
        assert_eq!(
            state.block_on_syscall(),
            BoundaryOutcome::Requeue,
            "S28: blocking here would lose the wake"
        );
        let snapshot = state.load();
        assert_eq!(snapshot.lifecycle(), TaskLifecycle::Ready);
        assert!(snapshot.is_queued());
        assert!(!snapshot.wake_requested());
    }

    #[test]
    fn s29_block_on_syscall_refuses_to_block_after_a_cancel() {
        let state = running();
        assert_eq!(state.request_cancel(), CancelOutcome::Deferred);
        assert_eq!(state.block_on_syscall(), BoundaryOutcome::Requeue, "S29");
        assert_eq!(state.lifecycle(), TaskLifecycle::Ready);
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);
    }

    #[test]
    fn s30_transitions_by_a_non_owner_are_refused() {
        for state in [AtomicTaskState::new(), parked(), blocked(), terminal()] {
            let before = state.load();
            assert_eq!(state.park_after_poll(), BoundaryOutcome::NotOwner, "I6");
            assert_eq!(state.block_on_syscall(), BoundaryOutcome::NotOwner, "I6");
            assert_eq!(state.requeue_after_poll(), BoundaryOutcome::NotOwner, "I6");
            assert!(
                !state.finish_terminal(),
                "I6: only the frame owner may publish a terminal transition"
            );
            assert_eq!(
                state.load(),
                before,
                "S30: a refused transition changes nothing"
            );
        }
    }

    #[test]
    fn s31_a_cancelling_task_resumes_cleanup_after_it_suspends() {
        let state = AtomicTaskState::new();
        state.request_cancel();
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);

        // A cleanup that itself has to wait parks like any other poll, and the
        // request is re-armed so the next claim resumes cleanup rather than the
        // task body.
        assert_eq!(state.park_after_poll(), BoundaryOutcome::Suspended);
        assert_eq!(state.lifecycle(), TaskLifecycle::Parked);
        assert!(state.load().cancel_requested(), "S31: re-armed");
        assert_eq!(state.wake(), WakeOutcome::Enqueue);
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel, "S31");
    }

    #[test]
    fn s32_the_claim_consumes_the_cancel_request() {
        let state = AtomicTaskState::new();
        state.request_cancel();
        assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);
        assert!(
            !state.load().cancel_requested(),
            "S32: the Cancelling state is now the record of the request"
        );
        // Without that, a cancelling task could never park: every boundary
        // would see the flag and requeue it forever.
        assert_eq!(state.park_after_poll(), BoundaryOutcome::Suspended);
    }

    #[test]
    fn s33_concurrent_cancel_and_park_never_strand_the_task() {
        for _ in 0..200 {
            let state = Arc::new(running());
            let pushes = Arc::new(AtomicUsize::new(0));
            let canceller = {
                let state = Arc::clone(&state);
                let pushes = Arc::clone(&pushes);
                std::thread::spawn(move || {
                    if state.request_cancel() == CancelOutcome::Enqueue {
                        pushes.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            let parker = {
                let state = Arc::clone(&state);
                let pushes = Arc::clone(&pushes);
                std::thread::spawn(move || {
                    if state.park_after_poll() == BoundaryOutcome::Requeue {
                        pushes.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            canceller.join().unwrap();
            parker.join().unwrap();

            let snapshot = state.load();
            assert_eq!(
                snapshot.lifecycle(),
                TaskLifecycle::Ready,
                "S33: whichever order they interleave, the cancel is observable"
            );
            assert!(snapshot.is_queued());
            assert!(snapshot.cancel_requested());
            assert_eq!(
                pushes.load(Ordering::Relaxed),
                1,
                "S33: exactly one thread owes the queue push"
            );
        }
    }

    #[test]
    fn s34_concurrent_wake_and_block_never_lose_the_wake() {
        for _ in 0..200 {
            let state = Arc::new(running());
            let pushes = Arc::new(AtomicUsize::new(0));
            let waker = {
                let state = Arc::clone(&state);
                let pushes = Arc::clone(&pushes);
                std::thread::spawn(move || {
                    if state.wake() == WakeOutcome::Enqueue {
                        pushes.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            let blocker = {
                let state = Arc::clone(&state);
                let pushes = Arc::clone(&pushes);
                std::thread::spawn(move || {
                    if state.block_on_syscall() == BoundaryOutcome::Requeue {
                        pushes.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            waker.join().unwrap();
            blocker.join().unwrap();

            let snapshot = state.load();
            assert_eq!(
                snapshot.lifecycle(),
                TaskLifecycle::Ready,
                "S34: the wake wins whether it lands before or after the block"
            );
            assert!(snapshot.is_queued());
            assert!(
                !snapshot.wake_requested(),
                "S34: consumed, not left pending"
            );
            assert_eq!(
                pushes.load(Ordering::Relaxed),
                1,
                "S34: exactly one thread owes the queue push"
            );
        }
    }

    #[test]
    fn s35_concurrent_cancel_and_blocking_completion_enqueue_once() {
        for _ in 0..200 {
            let state = Arc::new(blocked());
            let pushes = Arc::new(AtomicUsize::new(0));
            let canceller = {
                let state = Arc::clone(&state);
                let pushes = Arc::clone(&pushes);
                std::thread::spawn(move || {
                    if state.request_cancel() == CancelOutcome::Enqueue {
                        pushes.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            let completer = {
                let state = Arc::clone(&state);
                let pushes = Arc::clone(&pushes);
                std::thread::spawn(move || {
                    if state.wake() == WakeOutcome::Enqueue {
                        pushes.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            canceller.join().unwrap();
            completer.join().unwrap();

            let snapshot = state.load();
            assert_eq!(snapshot.lifecycle(), TaskLifecycle::Ready);
            assert!(snapshot.is_queued());
            assert!(snapshot.cancel_requested());
            assert_eq!(
                pushes.load(Ordering::Relaxed),
                1,
                "S35: cancellation and completion share one queue slot"
            );
            assert_eq!(state.claim_for_poll(), ClaimOutcome::Cancel);
        }
    }
}
