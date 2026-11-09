//! Channel FIFO with indexed links, so unlink/pop never scan tombstones or
//! compact a backing FIFO on the receive/select-cleanup path. Hash lookups
//! retain the runtime's expected O(1) bound. Storage is O(peak live waiters).
use crate::id_hash::IdMap;
use std::collections::hash_map::Entry as MapEntry;

#[derive(Debug, Clone, Copy)]
struct Entry {
    ticket: u64,
    previous: Option<u64>,
    next: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct WaiterQueue {
    // Most channels have one waiting producer or consumer. Keep that ticket
    // inline until a second distinct task arrives; no hash lookup/allocation
    // is needed for the usual register/wake/unregister round trip.
    single: Option<(u64, u64)>,
    entries: IdMap<u64, Entry>,
    head: Option<u64>,
    tail: Option<u64>,
    next_ticket: u64,
    native_table_bytes: usize,
}

#[cfg(test)]
thread_local! {
    pub(super) static LINK_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl WaiterQueue {
    pub(super) fn native_bytes(&mut self) -> usize {
        crate::native_memory::retain_capacity(
            &mut self.native_table_bytes,
            crate::native_memory::hash_bytes::<u64, Entry>(self.entries.capacity()),
        )
    }

    pub(super) fn register_ticket_fresh(&mut self, task: u64) -> (u64, bool) {
        let ticket = self.next_ticket;
        if self.entries.is_empty() {
            if let Some((id, generation)) = self.single {
                if id == task {
                    return (generation, false);
                }
                // Promote only one existing ticket, preserving its generation
                // and FIFO position. The normal insertion links the new tail.
                self.entries.insert(
                    id,
                    Entry {
                        ticket: generation,
                        previous: None,
                        next: None,
                    },
                );
                self.head = Some(id);
                self.tail = Some(id);
                self.single = None;
            } else {
                self.next_ticket = ticket
                    .checked_add(1)
                    .expect("channel waiter ticket exhausted");
                self.single = Some((task, ticket));
                return (ticket, true);
            }
        }
        match self.entries.entry(task) {
            MapEntry::Occupied(entry) => return (entry.get().ticket, false),
            MapEntry::Vacant(entry) => {
                self.next_ticket = ticket
                    .checked_add(1)
                    .expect("channel waiter ticket exhausted");
                entry.insert(Entry {
                    ticket,
                    previous: self.tail,
                    next: None,
                });
            }
        }
        if let Some(tail) = self.tail {
            self.entries.get_mut(&tail).unwrap().next = Some(task);
        } else {
            self.head = Some(task);
        }
        self.tail = Some(task);
        (ticket, true)
    }

    pub(super) fn ticket(&self, task: u64) -> Option<u64> {
        if let Some((id, ticket)) = self.single {
            return (id == task).then_some(ticket);
        }
        self.entries.get(&task).map(|entry| entry.ticket)
    }

    pub(super) fn remove_ticket(&mut self, task: u64, ticket: u64) -> bool {
        #[cfg(test)]
        LINK_VISITS.with(|count| count.set(count.get() + 1));
        if let Some(single) = self.single {
            if single == (task, ticket) {
                self.single = None;
                return true;
            }
            return false;
        }
        let entry = match self.entries.entry(task) {
            MapEntry::Occupied(entry) if entry.get().ticket == ticket => entry.remove(),
            _ => return false,
        };
        self.unlink(entry);
        true
    }

    fn unlink(&mut self, entry: Entry) {
        if let Some(previous) = entry.previous {
            #[cfg(test)]
            LINK_VISITS.with(|count| count.set(count.get() + 1));
            self.entries.get_mut(&previous).unwrap().next = entry.next;
        } else {
            self.head = entry.next;
        }
        if let Some(next) = entry.next {
            #[cfg(test)]
            LINK_VISITS.with(|count| count.set(count.get() + 1));
            self.entries.get_mut(&next).unwrap().previous = entry.previous;
        } else {
            self.tail = entry.previous;
        }
    }

    pub(super) fn pop_front_ticket(&mut self) -> Option<(u64, u64)> {
        if let Some(single) = self.single.take() {
            #[cfg(test)]
            LINK_VISITS.with(|count| count.set(count.get() + 1));
            return Some(single);
        }
        let task = self.head?;
        #[cfg(test)]
        LINK_VISITS.with(|count| count.set(count.get() + 1));
        let entry = self.entries.remove(&task).expect("indexed FIFO head");
        self.unlink(entry);
        Some((task, entry.ticket))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.single.is_none() && self.head.is_none()
    }

    pub(super) fn live(&self) -> Vec<u64> {
        if let Some((task, _)) = self.single {
            return vec![task];
        }
        let mut result = Vec::with_capacity(self.entries.len());
        let mut cursor = self.head;
        while let Some(task) = cursor {
            result.push(task);
            cursor = self.entries[&task].next;
        }
        result
    }

    #[cfg(test)]
    pub(super) fn register(&mut self, task: u64) -> bool {
        self.register_ticket_fresh(task).1
    }
    #[cfg(test)]
    pub(super) fn remove(&mut self, task: u64) -> bool {
        self.ticket(task)
            .is_some_and(|ticket| self.remove_ticket(task, ticket))
    }
    #[cfg(test)]
    pub(super) fn contains(&self, task: &u64) -> bool {
        self.ticket(*task).is_some()
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len() + usize::from(self.single.is_some())
    }
    #[cfg(test)]
    pub(super) fn queued_entries(&self) -> usize {
        self.len()
    }
    #[cfg(test)]
    pub(super) fn drain_all(&mut self) -> Vec<u64> {
        let mut result = Vec::with_capacity(self.entries.len());
        while let Some((task, _)) = self.pop_front_ticket() {
            result.push(task);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_waiter_churn_never_allocates_a_table() {
        for count in [1, 32, 1024, 32768] {
            let mut queue = WaiterQueue::default();
            LINK_VISITS.set(0);
            for task in 0..count {
                let (ticket, fresh) = queue.register_ticket_fresh(task);
                assert!(fresh);
                assert_eq!(ticket, task);
                assert_eq!(queue.register_ticket_fresh(task), (ticket, false));
                assert_eq!(queue.ticket(task), Some(ticket));
                assert_eq!(queue.ticket(task + 1), None);
                assert_eq!(queue.live(), [task]);
                assert!(!queue.remove_ticket(task, ticket + 1));
                if task % 2 == 0 {
                    assert_eq!(queue.pop_front_ticket(), Some((task, ticket)));
                } else {
                    assert!(queue.remove_ticket(task, ticket));
                }
                assert!(queue.is_empty());
                assert_eq!(queue.native_bytes(), 0);
                assert_eq!(queue.entries.capacity(), 0);
            }
            assert_eq!(LINK_VISITS.get(), 2 * count as usize);
            println!(
                "single_waiters={count} link_visits={} table_capacity=0",
                LINK_VISITS.get()
            );
        }
    }

    #[test]
    fn promotion_preserves_tickets_and_draining_returns_to_inline_storage() {
        let mut queue = WaiterQueue::default();
        for _ in 0..32 {
            let (first, _) = queue.register_ticket_fresh(10);
            let (second, _) = queue.register_ticket_fresh(20);
            assert!(queue.single.is_none());
            assert_eq!(queue.register_ticket_fresh(10), (first, false));
            assert_eq!(queue.live(), [10, 20]);
            assert!(queue.native_bytes() > 0);
            let charged = queue.native_bytes();
            assert!(!queue.remove_ticket(10, first + 1));
            assert!(queue.remove_ticket(10, first));
            let (third, _) = queue.register_ticket_fresh(10);
            assert!(third > second);
            assert!(!queue.remove_ticket(10, first));
            assert_eq!(queue.pop_front_ticket(), Some((20, second)));
            assert_eq!(queue.pop_front_ticket(), Some((10, third)));
            assert_eq!(queue.pop_front_ticket(), None);
            assert!(queue.is_empty());
            let (fourth, _) = queue.register_ticket_fresh(10);
            assert_eq!(queue.single, Some((10, fourth)));
            assert_eq!(
                queue.native_bytes(),
                charged,
                "retain the allocated table charge"
            );
            assert!(queue.remove_ticket(10, fourth));
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn mixed_waiter_operations_match_fifo_model() {
        use std::collections::VecDeque;
        let mut queue = WaiterQueue::default();
        let mut model = VecDeque::new();
        let mut random = 1u64;
        let mut next_ticket = 0;
        for _ in 0..20_000 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let task = (random >> 32) % 16;
            match (random >> 48) % 4 {
                0 | 1 => {
                    let expected =
                        if let Some(&(_, ticket)) = model.iter().find(|&&(id, _)| id == task) {
                            (ticket, false)
                        } else {
                            let ticket = next_ticket;
                            next_ticket += 1;
                            model.push_back((task, ticket));
                            (ticket, true)
                        };
                    assert_eq!(queue.register_ticket_fresh(task), expected);
                }
                2 => assert_eq!(queue.pop_front_ticket(), model.pop_front()),
                _ => {
                    if let Some(index) = model.iter().position(|&(id, _)| id == task) {
                        let (_, ticket) = model.remove(index).unwrap();
                        assert!(!queue.remove_ticket(task, ticket + 1));
                        assert!(queue.remove_ticket(task, ticket));
                    } else {
                        assert!(!queue.remove_ticket(task, 0));
                    }
                }
            }
            assert_eq!(queue.len(), model.len());
            assert_eq!(queue.is_empty(), model.is_empty());
            assert_eq!(
                queue.live(),
                model.iter().map(|&(id, _)| id).collect::<Vec<_>>()
            );
            for task in 0..16 {
                assert_eq!(
                    queue.ticket(task),
                    model
                        .iter()
                        .find(|&&(id, _)| id == task)
                        .map(|&(_, ticket)| ticket)
                );
            }
        }
    }
}
