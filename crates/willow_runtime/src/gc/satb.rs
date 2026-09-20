//! Per-mutator deletion buffers. The owning heap mutex serializes producers
//! with STW drains; callbacks publish values only and never trace/allocate GC.
use std::collections::HashMap;
use std::thread::ThreadId;

pub(super) struct SatbBuffers {
    capacity: usize,
    entries: HashMap<ThreadId, Vec<usize>>,
}

impl Default for SatbBuffers {
    fn default() -> Self {
        let capacity = std::env::var("WILLOW_GC_SATB_BUFFER_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| (1..=65536).contains(n))
            .unwrap_or(256);
        Self::new(capacity)
    }
}

impl SatbBuffers {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.values().all(Vec::is_empty)
    }

    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            entries: HashMap::new(),
        }
    }

    pub(super) fn record(
        &mut self,
        thread: ThreadId,
        value: usize,
        mut publish: impl FnMut(&[usize]),
    ) {
        if value == 0 {
            return;
        }
        let entries = self
            .entries
            .entry(thread)
            .or_insert_with(|| Vec::with_capacity(self.capacity));
        entries.push(value);
        if entries.len() == self.capacity {
            publish(entries);
            entries.clear();
        }
    }

    pub(super) fn flush_thread(
        &mut self,
        thread: ThreadId,
        retire: bool,
        mut publish: impl FnMut(&[usize]),
    ) {
        if let Some(entries) = self.entries.get_mut(&thread)
            && !entries.is_empty()
        {
            publish(entries);
            entries.clear();
        }
        if retire {
            self.entries.remove(&thread);
        }
    }

    pub(super) fn flush_all(&mut self, mut publish: impl FnMut(&[usize])) {
        for entries in self.entries.values_mut() {
            if !entries.is_empty() {
                publish(entries);
                entries.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_one_and_partial_flush_publish_every_deleted_edge_once() {
        for capacity in [1, 2, 256] {
            for n in [1, 16, 257, 4096] {
                let mut buffers = SatbBuffers::new(capacity);
                let mut published = Vec::new();
                let thread = std::thread::current().id();
                for value in 1..=n {
                    buffers.record(thread, value, |v| published.extend_from_slice(v));
                    assert!(buffers.entries[&thread].len() < capacity);
                }
                buffers.flush_thread(thread, true, |v| published.extend_from_slice(v));
                buffers.flush_all(|v| published.extend_from_slice(v));
                assert_eq!(published, (1..=n).collect::<Vec<_>>());
                assert!(buffers.entries.is_empty());
                println!(
                    "satb capacity={capacity} deletions={n} published={}",
                    published.len()
                );
            }
        }
    }

    #[test]
    fn remark_drains_every_mutator_and_reuses_registered_buffers() {
        let ids: Vec<_> = (0..16)
            .map(|_| {
                std::thread::spawn(|| std::thread::current().id())
                    .join()
                    .unwrap()
            })
            .collect();
        let mut buffers = SatbBuffers::new(8);
        let mut published = Vec::new();
        for &id in &ids {
            buffers.record(id, 7, |v| published.extend_from_slice(v));
        }
        assert!(published.is_empty());
        buffers.flush_all(|v| published.extend_from_slice(v));
        assert_eq!(published, vec![7; ids.len()]);
        assert_eq!(buffers.entries.len(), ids.len());
        for &id in &ids {
            assert_eq!(buffers.entries[&id].capacity(), 8);
            buffers.flush_thread(id, true, |_| panic!("duplicate publication"));
        }
        assert!(buffers.entries.is_empty());
    }
}
