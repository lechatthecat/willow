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
        self.entries.get(&task).map(|entry| entry.ticket)
    }

    pub(super) fn remove_ticket(&mut self, task: u64, ticket: u64) -> bool {
        #[cfg(test)]
        LINK_VISITS.with(|count| count.set(count.get() + 1));
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
        let task = self.head?;
        #[cfg(test)]
        LINK_VISITS.with(|count| count.set(count.get() + 1));
        let entry = self.entries.remove(&task).expect("indexed FIFO head");
        self.unlink(entry);
        Some((task, entry.ticket))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub(super) fn live(&self) -> Vec<u64> {
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
        self.entries.contains_key(task)
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    #[cfg(test)]
    pub(super) fn queued_entries(&self) -> usize {
        self.entries.len()
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
