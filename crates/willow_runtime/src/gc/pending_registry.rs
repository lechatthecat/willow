//! Removable FIFO of pending registry keys. No sparse-table iteration or
//! processed-prefix scan occurs when a bounded collector resumes.
use std::collections::HashMap;
use std::hash::Hash;

pub(super) struct Pending<K> {
    links: HashMap<K, (Option<K>, Option<K>)>,
    head: Option<K>,
    tail: Option<K>,
}
impl<K> Default for Pending<K> {
    fn default() -> Self {
        Self {
            links: HashMap::new(),
            head: None,
            tail: None,
        }
    }
}
impl<K: Copy + Eq + Hash> Pending<K> {
    pub(super) fn front(&self) -> Option<K> {
        self.head
    }
    pub(super) fn insert(&mut self, key: K) {
        if self.links.contains_key(&key) {
            return;
        }
        if let Some(tail) = self.tail {
            self.links.get_mut(&tail).unwrap().1 = Some(key);
        } else {
            self.head = Some(key);
        }
        self.links.insert(key, (self.tail, None));
        self.tail = Some(key);
    }
    pub(super) fn remove(&mut self, key: K) -> bool {
        let Some((previous, next)) = self.links.remove(&key) else {
            return false;
        };
        if let Some(previous) = previous {
            self.links.get_mut(&previous).unwrap().1 = next;
        } else {
            self.head = next;
        }
        if let Some(next) = next {
            self.links.get_mut(&next).unwrap().0 = previous;
        } else {
            self.tail = previous;
        }
        true
    }
}
