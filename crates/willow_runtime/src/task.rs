use crate::task_state::{
    AtomicTaskState, BoundaryOutcome, ClaimOutcome, TaskLifecycle, WakeOutcome,
};
use crate::wait_queue::WaitQueue;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

pub type RuntimeTaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTaskState {
    Ready,
    Running,
    Parked,
    /// Detached from a scheduler worker while a bounded blocking-pool job owns
    /// the native syscall. A completion wake moves it back to Ready.
    BlockedSyscall,
    Completed,
    Panicked,
    /// Cancel-requested task whose cleanup entry (`cancel_fn`) is currently
    /// being run by a worker (willow-vynv.3). In-flight: not claimable, not
    /// done for awaiters.
    Cancelling,
    /// Cooperatively cancelled (willow-0a6k.7): the task was cancel-requested
    /// and reached a scheduler boundary without being polled again. Awaiting a
    /// cancelled task is a runtime panic.
    Cancelled,
}

/// A compiler-generated cooperative resume entry point. Given the task's heap
/// async frame, it advances the state machine and returns a [`RuntimePoll`] code
/// (`0` = Pending, `1` = Ready). It must not block; to wait it registers a wake
/// (timer/channel/dependency) and returns Pending. Re-entrant into the scheduler
/// is allowed (e.g. to spawn or wake other tasks) — the driver holds no borrow
/// across the call.
pub type RuntimePollFn = unsafe extern "C" fn(frame: *mut c_void) -> i32;

/// Compiler-generated cancellation cleanup entry (willow-vynv.3): runs the
/// task's still-pending `defer`s (reverse lexical order) against its frame.
/// Called by a worker WITHOUT the scheduler lock held, exactly like `poll`.
pub type RuntimeCancelFn = unsafe extern "C" fn(frame: *mut c_void);

/// Poll result codes returned by a [`RuntimePollFn`] (preemption spec §7).
///
/// `Pending`/`Ready` are the cooperative-async base codes; `Yield`/`Preempted`/
/// `Panicked` are the preemptible-task extension (willow-0a6k.1). `Yield` and
/// `Preempted` are both *runnable* outcomes (the scheduler requeues the task)
/// and differ only diagnostically: `Yield` is voluntary, `Preempted` is forced
/// by the runtime at a safepoint. They are emitted once compiler-inserted
/// safepoints land (willow-0a6k.2); the scheduler already honors them.
pub const RUNTIME_POLL_PENDING: i32 = 0;
pub const RUNTIME_POLL_READY: i32 = 1;
pub const RUNTIME_POLL_YIELD: i32 = 2;
pub const RUNTIME_POLL_PREEMPTED: i32 = 3;
pub const RUNTIME_POLL_PANICKED: i32 = 4;
/// The task submitted native blocking work and detached from this worker.
pub const RUNTIME_POLL_BLOCKED_SYSCALL: i32 = 5;

/// Wait relationships a task only owns while it is actually waiting on
/// something (willow-ezs.3). The overwhelmingly common case — a ready or
/// running task — has none of them, so they live behind one lazy allocation
/// instead of costing 160 inline bytes in every scheduler slot. The box is
/// released again as soon as the last relationship goes away, so a workload
/// that parks and resumes 10,000 tasks returns to its ready-task footprint.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskWaitLinks {
    /// Tasks parked awaiting THIS task's completion; woken in registration
    /// order when it completes (dependency wake for `await <task>`,
    /// willow-lpn.5.3). A `WaitQueue` rather than a `Vec` so 10,000 tasks
    /// awaiting one task register in expected O(1) each instead of O(n²)
    /// total, and so a cancelled waiter is removed without a scan
    /// (willow-ezs.2).
    waiters: WaitQueue<RuntimeTaskId>,
    /// Tasks THIS task is registered on as a completion waiter — the reverse of
    /// `waiters`, mirroring `wait_channels`. Cancellation walks it to remove
    /// this task from those waiter lists in O(registered); without it a
    /// cancelled select task waiter lingers until its awaitee finishes
    /// (willow-o038 review). A set, not a queue: nothing depends on the order
    /// in which one task's awaitees are visited (willow-ezs.2).
    awaiting: HashSet<RuntimeTaskId>,
    /// Channels this task is registered on as a recv/select waiter — reverse
    /// references so cancellation deregisters in O(registered) instead of
    /// scanning every channel (willow-p4er). Addresses stay live while the
    /// task does (the handles sit in its rooted frame).
    wait_channels: Vec<usize>,
}

impl TaskWaitLinks {
    /// Whether every relationship is gone, including waiter tombstones, so the
    /// owning task can drop the allocation.
    fn is_vacant(&self) -> bool {
        self.waiters.is_empty() && self.awaiting.is_empty() && self.wait_channels.is_empty()
    }
}

/// Debug metadata the compiler tags onto a task in debug builds
/// (willow-ezs.3). Release-mode tasks and runtime-internal placeholders never
/// carry it, so it is lazily allocated separately from [`TaskWaitLinks`]: a
/// task that waits should not pay for debug strings, and a debug-tagged task
/// that never waits should not pay for wait queues.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskDebugInfo {
    /// Source name of the async fn this task runs, tagged at poll-fn entry.
    /// Used to render async stack traces from the suspended future chain
    /// (willow-9lw).
    name: Option<String>,
    /// Source location of the call that spawned this task (file, line), for
    /// panic/debug traces (willow-0a6k.7).
    spawn_site: Option<(String, u32)>,
}

#[derive(Debug)]
pub struct RuntimeTask {
    pub id: RuntimeTaskId,
    /// Atomic lifecycle and run-queue ownership. This is the only source of
    /// truth for Ready/Running/Parked/BlockedSyscall/Cancelling and replaces
    /// the former state + wake/cancel/in-queue fields (willow-x3te).
    pub(crate) state: AtomicTaskState,
    /// Cooperative resume entry, or `None` for a bookkeeping-only placeholder.
    pub poll: Option<RuntimePollFn>,
    /// Cancellation cleanup entry running the frame's pending defers, or
    /// `None` when the async fn has no defer sites (willow-vynv.3).
    pub cancel: Option<RuntimeCancelFn>,
    /// Heap async frame passed to `poll`. Kept alive via a GC runtime root while
    /// the task is pending; `null` for placeholders.
    pub frame: *mut c_void,
    /// Whether `frame` still owns the runtime root installed at spawn. Completed
    /// frames stay rooted until the outer scheduler drive ends so an awaiter on
    /// another worker can copy the result before a concurrent collection.
    pub frame_rooted: bool,
    /// When `Some`, this (parked) task should be woken once the instant passes —
    /// set by `willow_sched_sleep` from a poll fn before it returns Pending, and
    /// honored by the timer-aware run loop (willow-lpn.5.3).
    pub wake_deadline: Option<std::time::Instant>,
    /// `await yield()` requested a cooperative requeue while the task was still
    /// Running. The scheduler publishes that requeue only after the poll returns
    /// Pending, avoiding a second worker polling the same frame concurrently.
    pub yield_requested: bool,
    /// Wait relationships, allocated on the first registration and released
    /// with the last (willow-ezs.3). Private so every mutation goes through the
    /// accessors that maintain that invariant.
    wait: Option<Box<TaskWaitLinks>>,
    /// Debug name and spawn site, allocated on the first tag (willow-ezs.3).
    debug: Option<Box<TaskDebugInfo>>,
    /// Stable per-task preemption request flag. Boxed so its address remains
    /// valid while the scheduler releases its lock and polls the task.
    preempt_flag: Box<AtomicBool>,
}

// SAFETY: `RuntimeTask` is only moved between worker threads inside the global
// scheduler mutex. Its raw frame pointer refers to a GC-managed async frame that
// is kept alive by a runtime root while the task is pending/running; generated
// code may move a task between workers only after the Send/Sync checks.
unsafe impl Send for RuntimeTask {}

impl Clone for RuntimeTask {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            state: self.state.clone(),
            poll: self.poll,
            cancel: self.cancel,
            frame: self.frame,
            frame_rooted: self.frame_rooted,
            wake_deadline: self.wake_deadline,
            yield_requested: self.yield_requested,
            wait: self.wait.clone(),
            debug: self.debug.clone(),
            preempt_flag: Box::new(AtomicBool::new(self.preempt_flag.load(Ordering::Acquire))),
        }
    }
}

impl RuntimeTask {
    pub fn new(id: RuntimeTaskId) -> Self {
        Self {
            id,
            state: AtomicTaskState::new(),
            poll: None,
            cancel: None,
            frame: std::ptr::null_mut(),
            frame_rooted: false,
            wake_deadline: None,
            yield_requested: false,
            wait: None,
            debug: None,
            preempt_flag: Box::new(AtomicBool::new(false)),
        }
    }

    // ── Lazy wait relationships (willow-ezs.3) ──────────────────────────────
    //
    // Every mutator goes through `wait_mut` (which allocates on demand) and
    // ends with `release_wait_if_vacant` (which frees again), so a task that
    // parks and resumes returns to the inline-only footprint. Readers never
    // allocate: a task with no `wait` box has no relationships by definition.

    fn wait_mut(&mut self) -> &mut TaskWaitLinks {
        self.wait.get_or_insert_with(Box::default)
    }

    fn release_wait_if_vacant(&mut self) {
        if self.wait.as_ref().is_some_and(|wait| wait.is_vacant()) {
            self.wait = None;
        }
    }

    /// Register `waiter` as parked on this task's completion. Returns whether
    /// the relationship is new; a duplicate registration is ignored and leaves
    /// `waiter` at its existing FIFO position, so it neither creates a second
    /// wake nor loses its place. Only an explicit `remove_waiter` followed by a
    /// fresh `register_waiter` moves a waiter to the tail.
    pub fn register_waiter(&mut self, waiter: RuntimeTaskId) -> bool {
        self.wait_mut().waiters.register(waiter)
    }

    /// Drop `waiter`'s registration. Returns whether one was live.
    pub fn remove_waiter(&mut self, waiter: RuntimeTaskId) -> bool {
        let removed = match self.wait.as_mut() {
            Some(wait) => wait.waiters.remove(waiter),
            None => false,
        };
        self.release_wait_if_vacant();
        removed
    }

    /// Take every live waiter in registration order, leaving none behind.
    pub fn take_waiters(&mut self) -> Vec<RuntimeTaskId> {
        let waiters = match self.wait.as_mut() {
            Some(wait) => wait.waiters.drain_all(),
            None => Vec::new(),
        };
        self.release_wait_if_vacant();
        waiters
    }

    /// Live waiters in registration order, without consuming them.
    pub fn live_waiters(&self) -> Vec<RuntimeTaskId> {
        match self.wait.as_ref() {
            Some(wait) => wait.waiters.live(),
            None => Vec::new(),
        }
    }

    pub fn waiter_count(&self) -> usize {
        self.wait.as_ref().map_or(0, |wait| wait.waiters.len())
    }

    pub fn has_waiters(&self) -> bool {
        self.waiter_count() > 0
    }

    /// Queue slots including tombstones — the tombstone-growth assertion.
    pub fn queued_waiter_entries(&self) -> usize {
        self.wait
            .as_ref()
            .map_or(0, |wait| wait.waiters.queued_entries())
    }

    /// First live waiter, for panic diagnostics only; may scan tombstones.
    pub fn first_live_waiter(&self) -> Option<RuntimeTaskId> {
        self.wait
            .as_ref()
            .and_then(|wait| wait.waiters.first_live())
    }

    /// Record that this task is registered on `awaitee`'s waiter list.
    pub fn add_awaiting(&mut self, awaitee: RuntimeTaskId) -> bool {
        self.wait_mut().awaiting.insert(awaitee)
    }

    pub fn remove_awaiting(&mut self, awaitee: RuntimeTaskId) -> bool {
        let removed = match self.wait.as_mut() {
            Some(wait) => wait.awaiting.remove(&awaitee),
            None => false,
        };
        self.release_wait_if_vacant();
        removed
    }

    /// Take every awaitee this task is registered on, leaving none behind. The
    /// order is unspecified: nothing depends on the order awaitees are visited.
    pub fn take_awaiting(&mut self) -> Vec<RuntimeTaskId> {
        let awaiting = match self.wait.as_mut() {
            Some(wait) => wait.awaiting.drain().collect(),
            None => Vec::new(),
        };
        self.release_wait_if_vacant();
        awaiting
    }

    pub fn is_awaiting(&self, awaitee: RuntimeTaskId) -> bool {
        self.wait
            .as_ref()
            .is_some_and(|wait| wait.awaiting.contains(&awaitee))
    }

    pub fn awaiting_count(&self) -> usize {
        self.wait.as_ref().map_or(0, |wait| wait.awaiting.len())
    }

    /// Record a channel this task parked on. Returns whether the reverse
    /// reference is new.
    pub fn add_wait_channel(&mut self, address: usize) -> bool {
        let channels = &mut self.wait_mut().wait_channels;
        if channels.contains(&address) {
            return false;
        }
        channels.push(address);
        true
    }

    pub fn remove_wait_channel(&mut self, address: usize) {
        if let Some(wait) = self.wait.as_mut() {
            wait.wait_channels.retain(|&channel| channel != address);
        }
        self.release_wait_if_vacant();
    }

    /// Take every channel reverse reference, leaving none behind.
    pub fn take_wait_channels(&mut self) -> Vec<usize> {
        let channels = match self.wait.as_mut() {
            Some(wait) => std::mem::take(&mut wait.wait_channels),
            None => Vec::new(),
        };
        self.release_wait_if_vacant();
        channels
    }

    pub fn wait_channels(&self) -> &[usize] {
        match self.wait.as_ref() {
            Some(wait) => &wait.wait_channels,
            None => &[],
        }
    }

    /// Whether this task currently owns a wait-relationship allocation. Used by
    /// the footprint measurements and the plateau tests (willow-ezs.3).
    pub fn owns_wait_links(&self) -> bool {
        self.wait.is_some()
    }

    // ── Lazy debug metadata (willow-ezs.3) ──────────────────────────────────

    pub fn name(&self) -> Option<&str> {
        self.debug.as_ref().and_then(|debug| debug.name.as_deref())
    }

    pub fn set_name(&mut self, name: String) {
        self.debug.get_or_insert_with(Box::default).name = Some(name);
    }

    pub fn spawn_site(&self) -> Option<(&str, u32)> {
        self.debug
            .as_ref()
            .and_then(|debug| debug.spawn_site.as_ref())
            .map(|(file, line)| (file.as_str(), *line))
    }

    pub fn set_spawn_site(&mut self, file: String, line: u32) {
        self.debug.get_or_insert_with(Box::default).spawn_site = Some((file, line));
    }

    /// Whether this task currently owns a debug-metadata allocation.
    pub fn owns_debug_info(&self) -> bool {
        self.debug.is_some()
    }

    /// Stable address passed to the worker's quantum lifecycle while polling.
    pub fn preempt_flag_ptr(&self) -> *const c_void {
        (&*self.preempt_flag as *const AtomicBool).cast()
    }

    /// The live state of this record, or `None` once it is terminal.
    ///
    /// Terminal is not representable: the lifecycle collapses Completed,
    /// Panicked and Cancelled into one bit pattern and the outcome lives on the
    /// frame, not here. A terminal record is short-lived but *is* observable —
    /// the scheduler publishes Terminal under the task's shard, releases it, and
    /// re-takes the shard to reap the record — so callers must handle `None`
    /// rather than assume they can only ever see a live task.
    pub fn runtime_state(&self) -> Option<RuntimeTaskState> {
        match self.state.lifecycle() {
            TaskLifecycle::Ready => Some(RuntimeTaskState::Ready),
            TaskLifecycle::Running => Some(RuntimeTaskState::Running),
            TaskLifecycle::Parked => Some(RuntimeTaskState::Parked),
            TaskLifecycle::BlockedSyscall => Some(RuntimeTaskState::BlockedSyscall),
            TaskLifecycle::Cancelling => Some(RuntimeTaskState::Cancelling),
            TaskLifecycle::Terminal => None,
        }
    }

    pub fn claim_for_poll(&self) -> ClaimOutcome {
        self.state.claim_for_poll()
    }

    pub fn park_after_poll(&self) -> BoundaryOutcome {
        self.state.park_after_poll()
    }

    pub fn wake(&self) -> WakeOutcome {
        self.state.wake()
    }
}

// A task owns no GC roots of its own (willow-ezs.3). Its one reachable
// GC-managed value is the async `frame`, which is kept alive through the
// runtime root registry (`willow_gc_add_runtime_root`) while `frame_rooted` is
// set — not through a per-task root set. The former `RuntimeTask::roots` field
// and its `GcTrace` impl were never populated by the runtime or by generated
// code, so they cost 24 bytes per task while suggesting a tracing path that
// collection never took.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_transitions_are_explicit() {
        let task = RuntimeTask::new(7);
        assert!(task.state.claim_queue_slot());
        assert_eq!(task.claim_for_poll(), ClaimOutcome::Poll);
        assert_eq!(task.park_after_poll(), BoundaryOutcome::Suspended);
        assert_eq!(task.runtime_state(), Some(RuntimeTaskState::Parked));
        assert_eq!(task.wake(), WakeOutcome::Enqueue);
        assert_eq!(task.runtime_state(), Some(RuntimeTaskState::Ready));
        assert_eq!(task.claim_for_poll(), ClaimOutcome::Poll);
        assert!(task.state.finish_terminal());
        assert_eq!(task.state.lifecycle(), TaskLifecycle::Terminal);
        assert_eq!(task.runtime_state(), None, "terminal is not representable");
    }

    #[test]
    fn cloned_task_owns_an_independent_preempt_flag() {
        let task = RuntimeTask::new(1);
        let cloned = task.clone();
        assert_ne!(task.preempt_flag_ptr(), cloned.preempt_flag_ptr());

        crate::preempt::willow_preempt_request(task.preempt_flag_ptr());
        assert_eq!(
            crate::preempt::willow_preempt_requested(task.preempt_flag_ptr()),
            1
        );
        assert_eq!(
            crate::preempt::willow_preempt_requested(cloned.preempt_flag_ptr()),
            0
        );
    }
}

/// Footprint and lazy-metadata tests for willow-ezs.3.
///
/// Test perspectives (F1–F22):
///
/// ```text
/// F1  a fresh task allocates neither the wait box nor the debug box
/// F2  the inline RuntimeTask stays at or below its recorded budget
/// F3  registering a waiter allocates the wait box once, not per waiter
/// F4  removing the last waiter releases the wait box again
/// F5  removing one of several waiters keeps the box
/// F6  the reverse `awaiting` relation allocates and releases the same way
/// F7  a channel reverse reference allocates and releases the same way
/// F8  mixed relationships release only when the LAST one goes away
/// F9  waiter tombstones keep the box alive only while a live member remains
/// F10 debug tagging is independent: a named task owns no wait box
/// F11 waiting is independent: a waiting task owns no debug box
/// F12 name and spawn site share one debug box
/// F13 re-tagging a name does not allocate a second box
/// F14 readers on an untagged/unwaiting task allocate nothing
/// F15 duplicate waiter registration reports "not new" and keeps one relation
/// F16 re-registration after removal moves the waiter to the FIFO tail
/// F17 taking waiters drains and releases in one step
/// F18 taking awaitees drains and releases in one step
/// F19 taking channel waits drains and releases in one step
/// F20 duplicate channel registration is deduplicated
/// F21 clone copies relationships and debug data without sharing them
/// F22 10,000 park/resume cycles return to the ready-task footprint
/// ```
#[cfg(test)]
mod footprint {
    use super::*;

    /// Inline size budget for one scheduler task slot. Before willow-ezs.3 the
    /// struct was 264 bytes: a dead `GcRootSet` (24), a dead `spawned_from`
    /// (24) and a dead `stack_trace` (24), plus 72 bytes of wait relationships
    /// and 56 bytes of debug metadata carried by every task whether or not it
    /// waited or was tagged. Both cold groups are now lazily boxed.
    const INLINE_BUDGET: usize = 128;

    #[test]
    fn f01_f02_fresh_task_is_inline_only_and_within_budget() {
        let task = RuntimeTask::new(1);
        assert!(!task.owns_wait_links(), "F1: no wait box before waiting");
        assert!(!task.owns_debug_info(), "F1: no debug box before tagging");
        let inline = std::mem::size_of::<RuntimeTask>();
        assert!(
            inline <= INLINE_BUDGET,
            "F2: RuntimeTask grew to {inline} bytes, over the {INLINE_BUDGET}-byte budget"
        );
    }

    #[test]
    fn f03_f04_waiters_allocate_once_and_release_with_the_last_one() {
        let mut task = RuntimeTask::new(1);
        assert!(task.register_waiter(10), "F3: first registration is new");
        assert!(task.owns_wait_links());
        task.register_waiter(11);
        task.register_waiter(12);
        assert_eq!(task.waiter_count(), 3, "F3: one box holds every waiter");

        assert!(task.remove_waiter(10));
        assert!(task.remove_waiter(11));
        assert!(task.owns_wait_links(), "F5: a live waiter keeps the box");
        assert!(task.remove_waiter(12));
        assert!(
            !task.owns_wait_links(),
            "F4: the last removal frees the box"
        );
    }

    #[test]
    fn f06_awaiting_allocates_and_releases() {
        let mut task = RuntimeTask::new(1);
        assert!(task.add_awaiting(7));
        assert!(task.owns_wait_links());
        assert!(task.is_awaiting(7));
        assert_eq!(task.awaiting_count(), 1);
        assert!(task.remove_awaiting(7));
        assert!(!task.owns_wait_links());
        assert!(!task.is_awaiting(7));
    }

    #[test]
    fn f07_channel_waits_allocate_and_release() {
        let mut task = RuntimeTask::new(1);
        assert!(task.add_wait_channel(0x1000));
        assert!(task.owns_wait_links());
        assert_eq!(task.wait_channels(), &[0x1000]);
        task.remove_wait_channel(0x1000);
        assert!(!task.owns_wait_links());
        assert!(task.wait_channels().is_empty());
    }

    #[test]
    fn f08_mixed_relationships_release_only_with_the_last_one() {
        let mut task = RuntimeTask::new(1);
        task.register_waiter(2);
        task.add_awaiting(3);
        task.add_wait_channel(0x2000);

        task.remove_waiter(2);
        assert!(task.owns_wait_links());
        task.remove_awaiting(3);
        assert!(task.owns_wait_links());
        task.remove_wait_channel(0x2000);
        assert!(!task.owns_wait_links(), "F8: released only after the last");
    }

    #[test]
    fn f09_tombstones_alone_do_not_keep_the_box_alive() {
        let mut task = RuntimeTask::new(1);
        task.register_waiter(2);
        task.register_waiter(3);
        task.remove_waiter(2);
        assert!(task.owns_wait_links(), "a live member remains");
        task.remove_waiter(3);
        assert!(
            !task.owns_wait_links(),
            "F9: tombstoned entries must not pin the allocation"
        );
    }

    #[test]
    fn f10_f11_wait_and_debug_data_are_independently_lazy() {
        let mut named = RuntimeTask::new(1);
        named.set_name("worker".to_string());
        assert!(named.owns_debug_info());
        assert!(
            !named.owns_wait_links(),
            "F10: naming allocates no wait box"
        );

        let mut waiting = RuntimeTask::new(2);
        waiting.register_waiter(9);
        assert!(waiting.owns_wait_links());
        assert!(
            !waiting.owns_debug_info(),
            "F11: waiting allocates no debug box"
        );
    }

    #[test]
    fn f12_f13_name_and_spawn_site_share_one_debug_box() {
        let mut task = RuntimeTask::new(1);
        task.set_name("first".to_string());
        task.set_spawn_site("src/main.wi".to_string(), 12);
        assert_eq!(task.name(), Some("first"));
        assert_eq!(task.spawn_site(), Some(("src/main.wi", 12)));

        task.set_name("second".to_string());
        assert_eq!(task.name(), Some("second"), "F13: retag overwrites");
        assert_eq!(
            task.spawn_site(),
            Some(("src/main.wi", 12)),
            "F12: retagging the name keeps the spawn site"
        );
    }

    #[test]
    fn f14_readers_on_a_bare_task_allocate_nothing() {
        let task = RuntimeTask::new(1);
        assert_eq!(task.waiter_count(), 0);
        assert!(!task.has_waiters());
        assert_eq!(task.queued_waiter_entries(), 0);
        assert_eq!(task.first_live_waiter(), None);
        assert!(task.live_waiters().is_empty());
        assert_eq!(task.awaiting_count(), 0);
        assert!(!task.is_awaiting(1));
        assert!(task.wait_channels().is_empty());
        assert_eq!(task.name(), None);
        assert_eq!(task.spawn_site(), None);
        assert!(!task.owns_wait_links(), "F14: reads must not allocate");
        assert!(!task.owns_debug_info(), "F14: reads must not allocate");
    }

    #[test]
    fn f15_f16_duplicate_registration_keeps_one_relation_in_place() {
        let mut task = RuntimeTask::new(1);
        task.register_waiter(10);
        task.register_waiter(11);
        assert!(
            !task.register_waiter(10),
            "F15: a duplicate is not a new relation"
        );
        assert_eq!(task.waiter_count(), 2);
        assert_eq!(
            task.live_waiters(),
            vec![10, 11],
            "F15: a duplicate must not reorder an existing registration"
        );

        // Unregistering first is what moves a waiter: the new registration
        // takes a fresh ticket at the tail rather than reviving the old slot.
        task.remove_waiter(10);
        task.register_waiter(10);
        assert_eq!(
            task.live_waiters(),
            vec![11, 10],
            "F16: re-registration appends at the FIFO tail"
        );
    }

    #[test]
    fn f17_f18_f19_taking_relationships_drains_and_releases() {
        let mut task = RuntimeTask::new(1);
        task.register_waiter(2);
        task.register_waiter(3);
        assert_eq!(task.take_waiters(), vec![2, 3], "F17");
        assert!(!task.owns_wait_links());

        task.add_awaiting(4);
        let mut awaiting = task.take_awaiting();
        awaiting.sort_unstable();
        assert_eq!(awaiting, vec![4], "F18");
        assert!(!task.owns_wait_links());

        task.add_wait_channel(0x30);
        assert_eq!(task.take_wait_channels(), vec![0x30], "F19");
        assert!(!task.owns_wait_links());
    }

    #[test]
    fn f20_duplicate_channel_registration_is_deduplicated() {
        let mut task = RuntimeTask::new(1);
        assert!(task.add_wait_channel(0x40));
        assert!(!task.add_wait_channel(0x40), "F20: already registered");
        assert_eq!(task.wait_channels(), &[0x40]);
    }

    #[test]
    fn f21_clone_copies_relationships_without_sharing_them() {
        let mut task = RuntimeTask::new(1);
        task.register_waiter(5);
        task.add_awaiting(6);
        task.add_wait_channel(0x50);
        task.set_name("origin".to_string());

        let mut cloned = task.clone();
        assert_eq!(cloned.live_waiters(), vec![5]);
        assert!(cloned.is_awaiting(6));
        assert_eq!(cloned.wait_channels(), &[0x50]);
        assert_eq!(cloned.name(), Some("origin"));

        cloned.remove_waiter(5);
        assert_eq!(
            task.live_waiters(),
            vec![5],
            "F21: the clone owns its own relationships"
        );
    }

    #[test]
    fn f22_park_resume_cycles_return_to_the_ready_footprint() {
        let mut task = RuntimeTask::new(1);
        for waiter in 0..10_000u64 {
            task.register_waiter(waiter);
            task.add_awaiting(waiter);
            task.add_wait_channel(waiter as usize);
            task.remove_waiter(waiter);
            task.remove_awaiting(waiter);
            task.remove_wait_channel(waiter as usize);
            assert!(
                !task.owns_wait_links(),
                "F22: cycle {waiter} retained a wait allocation"
            );
        }
    }
}
