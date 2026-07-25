//! FIFO wait queue with expected O(1) membership (willow-ezs.2).
//!
//! Both channels and task-completion waits need the same structure: a queue
//! that wakes waiters in registration order, answers "is this waiter already
//! registered?" without scanning, and removes a waiter from the middle when it
//! is cancelled or loses a `select` arm.
//!
//! A plain `Vec` gives O(n) `contains`/`retain`, so registering 10,000 waiters
//! on one channel or one awaitee is quadratic. A plain `HashSet` gives O(1)
//! membership but no wake order. This queue keeps both:
//!
//! * `order` is the FIFO of registrations, and may contain tombstones — entries
//!   whose waiter has since been removed or re-registered.
//! * `members` maps each live waiter to the ticket of its current entry, so a
//!   tombstone is exactly an entry whose ticket no longer matches.
//!
//! Removal is therefore O(1) (drop the map entry and leave the tombstone), and
//! `compact_if_sparse` bounds `order` by rebuilding once tombstones outnumber
//! live entries. Each compaction at least halves the queue, so the amortized
//! cost per removal stays O(1) and a long-lived channel with churning waiters
//! cannot grow without bound.
//!
//! Re-registering an already-removed waiter appends a fresh entry, so an
//! unregister/re-register pair (a losing `select` arm that parks again) moves
//! that waiter to the FIFO tail rather than keeping its original position.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaitEntry<T> {
    value: T,
    ticket: u64,
}

#[derive(Debug, Clone)]
pub struct WaitQueue<T> {
    order: VecDeque<WaitEntry<T>>,
    members: HashMap<T, u64>,
    next_ticket: u64,
}

impl<T> Default for WaitQueue<T> {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            members: HashMap::new(),
            next_ticket: 0,
        }
    }
}

impl<T: Copy + Eq + Hash> WaitQueue<T> {
    /// Register `value`. Returns false when it was already registered, so the
    /// caller can skip the reverse-reference bookkeeping that pairs with it.
    pub fn register(&mut self, value: T) -> bool {
        if self.members.contains_key(&value) {
            return false;
        }
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.members.insert(value, ticket);
        self.order.push_back(WaitEntry { value, ticket });
        true
    }

    /// Deregister `value` (select unregister, wake, cancellation purge).
    /// Returns whether it was registered.
    pub fn remove(&mut self, value: T) -> bool {
        let removed = self.members.remove(&value).is_some();
        if removed {
            self.compact_if_sparse();
        }
        removed
    }

    /// Take the oldest live waiter. Tombstones and superseded registrations are
    /// skipped, so a stale head cannot swallow a wake.
    pub fn pop_front(&mut self) -> Option<T> {
        while let Some(entry) = self.order.pop_front() {
            if self.members.get(&entry.value) == Some(&entry.ticket) {
                self.members.remove(&entry.value);
                return Some(entry.value);
            }
        }
        None
    }

    /// Take every live waiter, in registration order, clearing the queue.
    pub fn drain_all(&mut self) -> Vec<T> {
        let mut woken = Vec::with_capacity(self.members.len());
        while let Some(value) = self.pop_front() {
            woken.push(value);
        }
        woken
    }

    pub fn contains(&self, value: &T) -> bool {
        self.members.contains_key(value)
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Live waiters in registration order (tombstones skipped, no duplicates).
    pub fn live(&self) -> Vec<T> {
        self.order
            .iter()
            .filter(|entry| self.members.get(&entry.value) == Some(&entry.ticket))
            .map(|entry| entry.value)
            .collect()
    }

    /// Oldest live waiter without removing it. This scans tombstones, so it is
    /// for diagnostics (async stack traces) only — never the registration or
    /// unregistration hot path.
    pub fn first_live(&self) -> Option<T> {
        self.order
            .iter()
            .find(|entry| self.members.get(&entry.value) == Some(&entry.ticket))
            .map(|entry| entry.value)
    }

    /// Number of queued entries including tombstones. Tests use it to prove
    /// that churn cannot grow the queue without bound.
    pub fn queued_entries(&self) -> usize {
        self.order.len()
    }

    /// Drop tombstones once they outnumber live entries.
    fn compact_if_sparse(&mut self) {
        if self.order.len() <= 8 || self.order.len() <= 2 * self.members.len() {
            return;
        }
        self.order
            .retain(|entry| self.members.get(&entry.value).copied() == Some(entry.ticket));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Perspectives for the shared queue itself (willow-ezs.2). The scheduler
    // and channel modules test their own use of it on top of these.
    //   WQ1  a fresh queue is empty
    //   WQ2  registration reports newly-registered
    //   WQ3  duplicate registration is rejected and does not duplicate order
    //   WQ4  membership is answered without scanning (contains)
    //   WQ5  pop_front returns registration order
    //   WQ6  pop_front clears membership
    //   WQ7  remove reports whether the value was registered
    //   WQ8  removing the head does not swallow the next wake
    //   WQ9  removing a middle entry keeps the rest in order
    //   WQ10 unregister then re-register moves the value to the tail
    //   WQ11 drain_all returns every live waiter in order and empties the queue
    //   WQ12 drain_all skips removed waiters
    //   WQ13 live() skips tombstones and never duplicates
    //   WQ14 first_live skips tombstones
    //   WQ15 first_live is None once every entry is a tombstone
    //   WQ16 churn cannot grow the queue without bound (compaction)
    //   WQ17 compaction preserves order and membership
    //   WQ18 pop_front on an all-tombstone queue returns None and drains it
    //   WQ19 len counts live entries only
    //   WQ20 a ticket-superseded stale entry never resurrects a removed waiter
    //   WQ21 10k registrations then 10k removals leave an empty, bounded queue

    #[test]
    fn wq01_fresh_queue_is_empty() {
        let queue: WaitQueue<u64> = WaitQueue::default();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.queued_entries(), 0);
        assert_eq!(queue.first_live(), None);
    }

    #[test]
    fn wq02_registration_reports_new_membership() {
        let mut queue = WaitQueue::default();
        assert!(queue.register(7u64));
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn wq03_duplicate_registration_is_rejected() {
        let mut queue = WaitQueue::default();
        assert!(queue.register(7u64));
        assert!(!queue.register(7u64));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.queued_entries(), 1);
        assert_eq!(queue.live(), vec![7]);
    }

    #[test]
    fn wq04_contains_answers_membership() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        assert!(queue.contains(&1));
        assert!(!queue.contains(&2));
    }

    #[test]
    fn wq05_pop_front_is_fifo() {
        let mut queue = WaitQueue::default();
        for id in [1u64, 2, 3] {
            queue.register(id);
        }
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn wq06_pop_front_clears_membership() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        assert_eq!(queue.pop_front(), Some(1));
        assert!(!queue.contains(&1));
        assert!(queue.is_empty());
    }

    #[test]
    fn wq07_remove_reports_previous_membership() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        assert!(queue.remove(1));
        assert!(!queue.remove(1));
        assert!(!queue.remove(99));
    }

    #[test]
    fn wq08_removed_head_does_not_swallow_the_next_wake() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        queue.register(2u64);
        queue.remove(1);
        assert_eq!(queue.pop_front(), Some(2));
    }

    #[test]
    fn wq09_removing_the_middle_keeps_the_rest_ordered() {
        let mut queue = WaitQueue::default();
        for id in [1u64, 2, 3] {
            queue.register(id);
        }
        queue.remove(2);
        assert_eq!(queue.live(), vec![1, 3]);
        assert_eq!(queue.drain_all(), vec![1, 3]);
    }

    #[test]
    fn wq10_reregistration_moves_the_value_to_the_tail() {
        let mut queue = WaitQueue::default();
        for id in [1u64, 2, 3] {
            queue.register(id);
        }
        queue.remove(1);
        assert!(queue.register(1));
        assert_eq!(queue.live(), vec![2, 3, 1]);
    }

    #[test]
    fn wq11_drain_all_empties_in_order() {
        let mut queue = WaitQueue::default();
        for id in [4u64, 5, 6] {
            queue.register(id);
        }
        assert_eq!(queue.drain_all(), vec![4, 5, 6]);
        assert!(queue.is_empty());
        assert_eq!(queue.queued_entries(), 0);
    }

    #[test]
    fn wq12_drain_all_skips_removed_waiters() {
        let mut queue = WaitQueue::default();
        for id in [1u64, 2, 3] {
            queue.register(id);
        }
        queue.remove(2);
        assert_eq!(queue.drain_all(), vec![1, 3]);
    }

    #[test]
    fn wq13_live_skips_tombstones_without_duplicates() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        queue.remove(1);
        queue.register(1u64);
        assert_eq!(queue.live(), vec![1]);
    }

    #[test]
    fn wq14_first_live_skips_tombstones() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        queue.register(2u64);
        queue.remove(1);
        assert_eq!(queue.first_live(), Some(2));
    }

    #[test]
    fn wq15_first_live_is_none_when_all_entries_are_tombstones() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        queue.register(2u64);
        queue.remove(1);
        queue.remove(2);
        assert_eq!(queue.first_live(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn wq16_churn_cannot_grow_the_queue_without_bound() {
        let mut queue = WaitQueue::default();
        queue.register(u64::MAX);
        for id in 0..10_000u64 {
            queue.register(id);
            queue.remove(id);
        }
        assert_eq!(queue.len(), 1);
        assert!(
            queue.queued_entries() <= 16,
            "tombstones must be compacted, saw {}",
            queue.queued_entries()
        );
    }

    #[test]
    fn wq17_compaction_preserves_order_and_membership() {
        let mut queue = WaitQueue::default();
        for id in 0..64u64 {
            queue.register(id);
        }
        for id in 0..64u64 {
            if id % 2 == 0 {
                queue.remove(id);
            }
        }
        let expected: Vec<u64> = (0..64).filter(|id| id % 2 == 1).collect();
        assert_eq!(queue.live(), expected);
        assert_eq!(queue.len(), expected.len());
        assert_eq!(queue.drain_all(), expected);
    }

    #[test]
    fn wq18_all_tombstone_queue_drains_to_none() {
        let mut queue = WaitQueue::default();
        for id in 0..4u64 {
            queue.register(id);
        }
        for id in 0..4u64 {
            queue.remove(id);
        }
        assert_eq!(queue.pop_front(), None);
        assert_eq!(queue.queued_entries(), 0);
    }

    #[test]
    fn wq19_len_counts_live_entries_only() {
        let mut queue = WaitQueue::default();
        for id in 0..5u64 {
            queue.register(id);
        }
        queue.remove(0);
        queue.remove(1);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn wq20_stale_entry_never_resurrects_a_removed_waiter() {
        let mut queue = WaitQueue::default();
        queue.register(1u64);
        queue.register(2u64);
        // Re-register 1: its original entry becomes a tombstone.
        queue.remove(1);
        queue.register(1u64);
        // Remove it again: the fresh entry becomes a tombstone too.
        queue.remove(1);
        assert_eq!(queue.drain_all(), vec![2]);
        assert!(!queue.contains(&1));
    }

    #[test]
    fn wq21_ten_thousand_registrations_then_removals_leave_nothing() {
        let mut queue = WaitQueue::default();
        for id in 0..10_000u64 {
            assert!(queue.register(id));
        }
        assert_eq!(queue.len(), 10_000);
        for id in 0..10_000u64 {
            assert!(queue.remove(id));
        }
        assert!(queue.is_empty());
        assert_eq!(queue.first_live(), None);
        assert!(queue.queued_entries() <= 16);
    }
}
