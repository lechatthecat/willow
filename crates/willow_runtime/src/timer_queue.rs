//! Timer deadlines with their own lock, off the scheduler metadata mutex
//! (willow-9ha4, stage 4 of `requirements/willow_scheduler_lock_decomposition.md`).
//!
//! # Why this is its own lock
//!
//! Every iteration of the worker run loop promotes expired timers before it
//! selects work, so before this module the loop took `GLOBAL_SCHEDULER` once
//! per poll on every worker even when no task had ever slept. That acquisition
//! was pure handoff cost: the critical section usually observed an empty heap
//! and did nothing.
//!
//! The heap now lives behind its own `Mutex`, and a lock-free `earliest_nanos`
//! hint lets the common "no timer is due" case answer with a single atomic
//! load.
//!
//! # The atomicity invariant this must preserve
//!
//! Popping a due timer and waking its task **must be one atomic step against an
//! observer that is deciding whether the scheduler is idle**. If a worker
//! removed the last timer, released the lock, and only then woke the task,
//! another worker could observe neither a pending timer nor runnable work and
//! wrongly return from `run_until` while the target task was still parked.
//! [`TimerQueue::wake_due`] therefore holds the heap lock across the `wake`
//! callback; that is deliberate and not an oversight.
//!
//! # Lock order
//!
//! ```text
//! TimerQueue::heap  ->  task shard
//! ```
//!
//! The `live` and `wake` callbacks run under the heap lock and take task
//! shards, so nothing may take the heap lock while holding a task shard.
//! [`TimerQueue::push`] is the one call that runs on the task side; it must be
//! made after the shard guard is released.
//!
//! # What the hint may and may not do
//!
//! [`TimerQueue::maybe_due`] is a *hint*, used only to skip the pre-claim
//! promotion in the run loop. It may report "due" when nothing is (the lock is
//! then taken and finds nothing — wasted work, never wrong), and it may report
//! "not due" for the instant a concurrent [`TimerQueue::push`] is between its
//! heap insert and its hint store. That stale-empty window cannot lose a wake:
//! every *idleness* decision goes through [`TimerQueue::next_deadline`], which
//! reads the heap under its lock and never consults the hint.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use crate::task::RuntimeTaskId;

/// `earliest_nanos` value meaning "the heap holds no timer".
const NO_DEADLINE: u64 = u64::MAX;

/// Fixed origin for the `Instant` -> `u64` projection used by the lock-free
/// hint. `Instant` is opaque and not directly representable in an atomic, so
/// deadlines are stored as nanoseconds since the first use of the timer queue.
static TIMER_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Project an `Instant` onto the hint's integer timeline.
///
/// Deadlines before the epoch saturate to 0 ("already due"), and the value is
/// clamped below [`NO_DEADLINE`] so a real deadline can never be mistaken for
/// the empty sentinel. u64 nanoseconds cover ~584 years of process uptime.
fn nanos_since_epoch(at: Instant) -> u64 {
    let nanos = at.saturating_duration_since(*TIMER_EPOCH).as_nanos();
    nanos.min(u128::from(NO_DEADLINE - 1)) as u64
}

/// One registered wake-deadline. Ordered by deadline first so the min-heap
/// (`Reverse<TimerWake>`) pops the earliest deadline; the task id breaks ties
/// deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TimerWake {
    pub deadline: Instant,
    pub task_id: RuntimeTaskId,
}

/// A min-heap of wake-deadlines plus a lock-free "earliest deadline" hint.
#[derive(Debug)]
pub(crate) struct TimerQueue {
    heap: Mutex<BinaryHeap<Reverse<TimerWake>>>,
    /// Nanoseconds-since-epoch of the earliest entry, or [`NO_DEADLINE`].
    /// Written only while `heap` is held; read without any lock.
    earliest_nanos: AtomicU64,
}

impl Default for TimerQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerQueue {
    pub(crate) fn new() -> Self {
        Self {
            heap: Mutex::new(BinaryHeap::new()),
            earliest_nanos: AtomicU64::new(NO_DEADLINE),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BinaryHeap<Reverse<TimerWake>>> {
        self.heap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Republish the hint from the heap the caller already holds.
    fn publish_hint(&self, heap: &BinaryHeap<Reverse<TimerWake>>) {
        let earliest = heap
            .peek()
            .map(|Reverse(wake)| nanos_since_epoch(wake.deadline))
            .unwrap_or(NO_DEADLINE);
        self.earliest_nanos.store(earliest, Ordering::Release);
    }

    /// Register `deadline` as `task_id`'s wake time.
    ///
    /// Call this only after any task shard guard has been released (see the
    /// module's lock order). A task may hold several entries — an entry whose
    /// deadline no longer matches the task's `wake_deadline` is pruned lazily
    /// by the `live` predicate rather than searched for and removed here, so
    /// re-arming a sleep stays O(log n) instead of O(n).
    pub(crate) fn push(&self, task_id: RuntimeTaskId, deadline: Instant) {
        let mut heap = self.lock();
        heap.push(Reverse(TimerWake { deadline, task_id }));
        self.publish_hint(&heap);
    }

    /// Lock-free hint: is it worth taking the lock to look for a due timer at
    /// `now`? See the module docs for exactly how imprecise this is allowed to
    /// be.
    pub(crate) fn maybe_due(&self, now: Instant) -> bool {
        self.earliest_nanos.load(Ordering::Acquire) <= nanos_since_epoch(now)
    }

    /// Drop leading entries that no longer describe a live wake, then report
    /// the earliest remaining one.
    ///
    /// `live` answers "does this task still expect this exact deadline?" — a
    /// task that was woken, re-armed, or finished leaves stale entries behind.
    pub(crate) fn next_deadline(
        &self,
        live: impl Fn(TimerWake) -> bool,
    ) -> Option<(RuntimeTaskId, Instant)> {
        let mut heap = self.lock();
        let next = Self::prune_locked(&mut heap, &live).map(|wake| (wake.task_id, wake.deadline));
        self.publish_hint(&heap);
        next
    }

    /// Pop every timer due at `now` and wake its task, all under one heap lock
    /// (see the atomicity invariant in the module docs). Returns how many
    /// timers were promoted.
    pub(crate) fn wake_due(
        &self,
        now: Instant,
        live: impl Fn(TimerWake) -> bool,
        mut wake: impl FnMut(RuntimeTaskId),
    ) -> usize {
        let mut heap = self.lock();
        let mut woken = 0;
        while let Some(wake_entry) = Self::prune_locked(&mut heap, &live) {
            if wake_entry.deadline > now {
                break;
            }
            heap.pop();
            wake(wake_entry.task_id);
            woken += 1;
        }
        self.publish_hint(&heap);
        woken
    }

    /// Discard leading stale entries and return the first live one, leaving it
    /// on the heap.
    fn prune_locked(
        heap: &mut BinaryHeap<Reverse<TimerWake>>,
        live: &impl Fn(TimerWake) -> bool,
    ) -> Option<TimerWake> {
        while let Some(Reverse(wake)) = heap.peek().copied() {
            if live(wake) {
                return Some(wake);
            }
            heap.pop();
        }
        None
    }

    /// Entries currently held, including not-yet-pruned stale ones.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Drop every entry (test reset).
    pub(crate) fn clear(&self) {
        let mut heap = self.lock();
        heap.clear();
        self.publish_hint(&heap);
    }

    /// The raw hint value, for tests that assert the fast path is armed.
    #[cfg(test)]
    pub(crate) fn earliest_hint_nanos(&self) -> u64 {
        self.earliest_nanos.load(Ordering::Acquire)
    }

    /// The hint's "no timers" sentinel, for tests.
    #[cfg(test)]
    pub(crate) const fn empty_hint() -> u64 {
        NO_DEADLINE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Every entry is live: the default for tests that do not model staleness.
    fn all_live(_wake: TimerWake) -> bool {
        true
    }

    /// An instant far enough after `base` that every deadline derived from
    /// `base` in these tests is due.
    ///
    /// Tests never subtract from `Instant::now()`: `Instant - Duration` panics
    /// if it would precede the platform's monotonic origin, which is reachable
    /// on a freshly booted machine. Deadlines are built forward from a base and
    /// the *observation* time is moved forward instead.
    fn well_past(base: Instant) -> Instant {
        base + Duration::from_secs(3_600)
    }

    fn collect_woken(queue: &TimerQueue, now: Instant) -> Vec<RuntimeTaskId> {
        let mut order = Vec::new();
        queue.wake_due(now, all_live, |id| order.push(id));
        order
    }

    // tq_01: an empty queue reports the empty sentinel and never claims a timer
    // is due.
    #[test]
    fn tq_01_empty_queue_has_no_deadline_and_no_hint() {
        let queue = TimerQueue::new();
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert!(!queue.maybe_due(Instant::now()));
        assert_eq!(queue.next_deadline(all_live), None);
    }

    // tq_02: a push publishes both the heap entry and the lock-free hint.
    #[test]
    fn tq_02_push_publishes_entry_and_hint() {
        let queue = TimerQueue::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        queue.push(7, deadline);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.earliest_hint_nanos(), nanos_since_epoch(deadline));
        assert_eq!(queue.next_deadline(all_live), Some((7, deadline)));
    }

    // tq_03: the heap is a MIN-heap — the earliest deadline is reported first
    // regardless of insertion order.
    #[test]
    fn tq_03_earliest_deadline_wins_regardless_of_insert_order() {
        let queue = TimerQueue::new();
        let now = Instant::now();
        queue.push(1, now + Duration::from_secs(30));
        queue.push(2, now + Duration::from_secs(10));
        queue.push(3, now + Duration::from_secs(20));
        assert_eq!(
            queue.next_deadline(all_live),
            Some((2, now + Duration::from_secs(10)))
        );
    }

    // tq_04: due timers fire in deadline order, not registration order.
    #[test]
    fn tq_04_due_timers_wake_in_deadline_order() {
        let queue = TimerQueue::new();
        let base = Instant::now();
        queue.push(30, base + Duration::from_millis(300));
        queue.push(10, base + Duration::from_millis(100));
        queue.push(20, base + Duration::from_millis(200));
        assert_eq!(collect_woken(&queue, well_past(base)), vec![10, 20, 30]);
        assert_eq!(queue.len(), 0);
    }

    // tq_05: a timer in the future is not promoted, and the queue keeps it.
    #[test]
    fn tq_05_future_timer_is_not_promoted() {
        let queue = TimerQueue::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        queue.push(1, deadline);
        assert_eq!(
            collect_woken(&queue, Instant::now()),
            Vec::<RuntimeTaskId>::new()
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.next_deadline(all_live), Some((1, deadline)));
    }

    // tq_06: a deadline exactly equal to `now` counts as due (the boundary is
    // inclusive, matching the pre-extraction `deadline > now` test).
    #[test]
    fn tq_06_deadline_equal_to_now_is_due() {
        let queue = TimerQueue::new();
        let now = Instant::now();
        queue.push(1, now);
        assert_eq!(collect_woken(&queue, now), vec![1]);
    }

    // tq_07: the hint arms as soon as the earliest deadline passes, so the run
    // loop's fast path takes the lock exactly when there is something to do.
    #[test]
    fn tq_07_hint_arms_once_the_deadline_passes() {
        let queue = TimerQueue::new();
        let deadline = Instant::now() + Duration::from_millis(5);
        queue.push(1, deadline);
        assert!(!queue.maybe_due(deadline - Duration::from_millis(1)));
        assert!(queue.maybe_due(deadline));
        assert!(queue.maybe_due(deadline + Duration::from_millis(1)));
    }

    // tq_08: draining every timer restores the empty sentinel, so an idle
    // scheduler goes back to the zero-lock fast path.
    #[test]
    fn tq_08_draining_restores_the_empty_hint() {
        let queue = TimerQueue::new();
        let base = Instant::now();
        queue.push(1, base);
        assert_eq!(collect_woken(&queue, well_past(base)), vec![1]);
        assert_eq!(queue.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert!(!queue.maybe_due(well_past(base)));
    }

    // tq_09: a stale entry (the task no longer expects that deadline) is pruned
    // by `next_deadline` and never reported.
    #[test]
    fn tq_09_next_deadline_prunes_stale_entries() {
        let queue = TimerQueue::new();
        let now = Instant::now();
        queue.push(1, now + Duration::from_secs(1));
        queue.push(2, now + Duration::from_secs(2));
        let live = |wake: TimerWake| wake.task_id != 1;
        assert_eq!(
            queue.next_deadline(live),
            Some((2, now + Duration::from_secs(2)))
        );
        assert_eq!(queue.len(), 1, "the stale head is dropped, not skipped");
    }

    // tq_10: a stale entry is never woken even when its deadline is due.
    #[test]
    fn tq_10_stale_due_entry_is_dropped_without_waking() {
        let queue = TimerQueue::new();
        let base = Instant::now();
        queue.push(1, base);
        queue.push(2, base + Duration::from_millis(1));
        let mut order = Vec::new();
        queue.wake_due(
            well_past(base),
            |wake| wake.task_id != 1,
            |id| order.push(id),
        );
        assert_eq!(order, vec![2]);
    }

    // tq_11: pruning also republishes the hint, so a queue holding only stale
    // entries stops claiming work exists.
    #[test]
    fn tq_11_pruning_republishes_the_hint() {
        let queue = TimerQueue::new();
        queue.push(1, Instant::now() + Duration::from_millis(1));
        assert_ne!(queue.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert_eq!(queue.next_deadline(|_| false), None);
        assert_eq!(queue.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert_eq!(queue.len(), 0);
    }

    // tq_12: re-arming a sleep leaves the superseded entry behind; the newer
    // deadline still wins and the old one is pruned when it reaches the head.
    #[test]
    fn tq_12_rearmed_task_keeps_only_its_current_deadline() {
        let queue = TimerQueue::new();
        let now = Instant::now();
        let old = now + Duration::from_secs(5);
        let new = now + Duration::from_secs(1);
        queue.push(1, old);
        queue.push(1, new);
        assert_eq!(queue.len(), 2);
        let live = move |wake: TimerWake| wake.deadline == new;
        assert_eq!(queue.next_deadline(live), Some((1, new)));
    }

    // tq_13: several tasks sharing one deadline all fire, ordered by task id.
    #[test]
    fn tq_13_ties_break_by_task_id() {
        let queue = TimerQueue::new();
        let deadline = Instant::now();
        queue.push(3, deadline);
        queue.push(1, deadline);
        queue.push(2, deadline);
        assert_eq!(collect_woken(&queue, well_past(deadline)), vec![1, 2, 3]);
    }

    // tq_14: `wake_due` stops at the first not-yet-due timer and leaves the
    // rest of the heap intact.
    #[test]
    fn tq_14_wake_due_stops_at_the_first_future_timer() {
        let queue = TimerQueue::new();
        let base = Instant::now();
        queue.push(1, base);
        queue.push(2, base + Duration::from_millis(1));
        queue.push(3, base + Duration::from_secs(7_200));
        let observed = well_past(base);
        assert_eq!(collect_woken(&queue, observed), vec![1, 2]);
        assert_eq!(queue.len(), 1);
        assert!(!queue.maybe_due(observed));
    }

    // tq_15: the wake callback runs while the heap lock is held, which is what
    // makes "pop + wake" atomic against an idleness observer.
    #[test]
    fn tq_15_wake_callback_runs_under_the_heap_lock() {
        let queue = TimerQueue::new();
        let base = Instant::now();
        queue.push(1, base);
        queue.wake_due(well_past(base), all_live, |_| {
            assert!(
                queue.heap.try_lock().is_err(),
                "the heap must still be locked while a timer is being woken"
            );
        });
    }

    // tq_16: `clear` empties the heap and the hint together (test reset path).
    #[test]
    fn tq_16_clear_empties_heap_and_hint() {
        let queue = TimerQueue::new();
        queue.push(1, Instant::now() + Duration::from_secs(1));
        queue.push(2, Instant::now() + Duration::from_secs(2));
        queue.clear();
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert_eq!(queue.next_deadline(all_live), None);
    }

    // tq_17: a deadline before the epoch saturates to "already due" instead of
    // wrapping into the far future.
    #[test]
    fn tq_17_pre_epoch_deadline_saturates_to_due() {
        let epoch = *LazyLock::force(&TIMER_EPOCH);
        assert_eq!(nanos_since_epoch(epoch), 0);
        // `Instant - Duration` panics near the platform's monotonic origin, so
        // the pre-epoch half of the check only runs where it is representable.
        let Some(before) = epoch.checked_sub(Duration::from_secs(1)) else {
            return;
        };
        assert_eq!(nanos_since_epoch(before), 0);
        let queue = TimerQueue::new();
        queue.push(1, before);
        assert!(queue.maybe_due(epoch));
        assert_eq!(collect_woken(&queue, epoch), vec![1]);
    }

    // tq_18: a real deadline can never collide with the empty sentinel.
    #[test]
    fn tq_18_projection_never_produces_the_empty_sentinel() {
        assert!(nanos_since_epoch(Instant::now()) < TimerQueue::empty_hint());
        assert!(
            nanos_since_epoch(Instant::now() + Duration::from_secs(60 * 60 * 24 * 365))
                < TimerQueue::empty_hint()
        );
    }

    // tq_19: concurrent pushes from many threads keep the heap and the hint
    // consistent, and every entry survives.
    #[test]
    fn tq_19_concurrent_pushes_keep_heap_and_hint_consistent() {
        let queue = Arc::new(TimerQueue::new());
        let base = Instant::now() + Duration::from_secs(60);
        std::thread::scope(|scope| {
            for thread in 0..8u64 {
                let queue = Arc::clone(&queue);
                scope.spawn(move || {
                    for step in 0..250u64 {
                        let id = thread * 1_000 + step + 1;
                        queue.push(id, base + Duration::from_millis(id));
                    }
                });
            }
        });
        assert_eq!(queue.len(), 8 * 250);
        let (_, earliest) = queue.next_deadline(all_live).expect("timers were pushed");
        assert_eq!(queue.earliest_hint_nanos(), nanos_since_epoch(earliest));
    }

    // tq_20: concurrent drains wake every due timer exactly once — no timer is
    // lost and none fires twice.
    #[test]
    fn tq_20_concurrent_drains_wake_each_timer_exactly_once() {
        let queue = Arc::new(TimerQueue::new());
        let base = Instant::now();
        for id in 1..=2_000u64 {
            queue.push(id, base);
        }
        let woken = Arc::new(AtomicUsize::new(0));
        let observed = well_past(base);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let queue = Arc::clone(&queue);
                let woken = Arc::clone(&woken);
                scope.spawn(move || {
                    queue.wake_due(observed, all_live, |_| {
                        woken.fetch_add(1, Ordering::Relaxed);
                    });
                });
            }
        });
        assert_eq!(woken.load(Ordering::Relaxed), 2_000);
        assert_eq!(queue.len(), 0);
    }

    // tq_21: a push racing a drain is never swallowed — the pusher's timer is
    // either promoted by the drain or still present afterwards.
    #[test]
    fn tq_21_push_racing_a_drain_is_not_lost() {
        for _ in 0..64 {
            let queue = Arc::new(TimerQueue::new());
            let base = Instant::now();
            let observed = well_past(base);
            let woken = Arc::new(AtomicUsize::new(0));
            std::thread::scope(|scope| {
                let pusher = Arc::clone(&queue);
                scope.spawn(move || {
                    for id in 1..=100u64 {
                        pusher.push(id, base);
                    }
                });
                let drainer = Arc::clone(&queue);
                let counter = Arc::clone(&woken);
                scope.spawn(move || {
                    for _ in 0..100 {
                        drainer.wake_due(observed, all_live, |_| {
                            counter.fetch_add(1, Ordering::Relaxed);
                        });
                    }
                });
            });
            let remaining = queue.len();
            assert_eq!(
                woken.load(Ordering::Relaxed) + remaining,
                100,
                "every pushed timer is either woken or still queued"
            );
        }
    }

    // tq_22: the hint is never LATER than the true earliest deadline once the
    // queue is quiescent — a hint that ran ahead would skip a due timer.
    #[test]
    fn tq_22_quiescent_hint_never_runs_ahead_of_the_heap() {
        let queue = TimerQueue::new();
        let now = Instant::now();
        for id in 1..=64u64 {
            queue.push(id, now + Duration::from_millis(100 - id));
        }
        let (_, earliest) = queue.next_deadline(all_live).expect("timers were pushed");
        assert!(queue.earliest_hint_nanos() <= nanos_since_epoch(earliest));
    }

    // tq_23: `wake_due` on an empty queue is a no-op that still leaves the
    // sentinel in place (the run loop calls it unconditionally).
    #[test]
    fn tq_23_wake_due_on_empty_queue_is_a_noop() {
        let queue = TimerQueue::new();
        assert_eq!(
            collect_woken(&queue, Instant::now()),
            Vec::<RuntimeTaskId>::new()
        );
        assert_eq!(queue.earliest_hint_nanos(), TimerQueue::empty_hint());
    }

    // tq_24: a queue holding only future timers reports a deadline but wakes
    // nothing, which is the "sleep until the deadline" idle path.
    #[test]
    fn tq_24_future_only_queue_reports_a_deadline_but_wakes_nothing() {
        let queue = TimerQueue::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        queue.push(9, deadline);
        assert_eq!(queue.next_deadline(all_live), Some((9, deadline)));
        assert_eq!(
            collect_woken(&queue, Instant::now()),
            Vec::<RuntimeTaskId>::new()
        );
        assert_eq!(queue.len(), 1);
    }
}
