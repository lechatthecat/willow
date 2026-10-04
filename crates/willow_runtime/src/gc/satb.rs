//! Per-mutator deletion buffers. The owning heap mutex serializes producers
//! with STW drains; callbacks publish values only and never trace/allocate GC.
//! `SatbShards` are per-cycle buffers that concurrent-mark barriers fill under
//! a shard mutex instead of the heap mutex (willow-jz15.53).
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::ThreadId;

fn configured_capacity() -> usize {
    std::env::var("WILLOW_GC_SATB_BUFFER_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| (1..=65536).contains(n))
        .unwrap_or(256)
}

pub(super) struct SatbBuffers {
    capacity: usize,
    entries: HashMap<ThreadId, Vec<usize>>,
}

impl Default for SatbBuffers {
    fn default() -> Self {
        Self::new(configured_capacity())
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

/// Fixed shard count: threads beyond it share shards round-robin, so memory
/// is O(shards * capacity) per cycle regardless of the mutator count.
const SATB_SHARDS: usize = 64;

thread_local! {
    static SHARD: usize = {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed) % SATB_SHARDS
    };
}

/// Per-cycle deletion buffers published without the heap mutex.
///
/// A producer locks its shard, then checks `sealed` and the caller's cycle
/// validity before publishing anything; publication (including the eager
/// enqueue of a full buffer) completes under that shard lock. `seal` stores
/// `sealed` before locking each shard, so every shard publication either
/// precedes the seal's flush of that shard or observes `sealed` and falls
/// back to the heap-locked path. Lock order: heap -> shard -> mark queue.
pub(super) struct SatbShards {
    capacity: usize,
    sealed: AtomicBool,
    shards: Box<[Shard]>,
}

/// Padded so neighbouring threads' shard locks do not share a cache line.
#[repr(align(128))]
struct Shard(Mutex<Vec<usize>>);

impl Default for SatbShards {
    fn default() -> Self {
        Self::new(configured_capacity())
    }
}

impl SatbShards {
    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            sealed: AtomicBool::new(false),
            shards: (0..SATB_SHARDS)
                .map(|_| Shard(Mutex::new(Vec::new())))
                .collect(),
        }
    }

    /// Run `publish` with this thread's shard buffer when the shards are
    /// unsealed and `valid()` holds under the shard lock; otherwise return
    /// `None` so the caller takes the heap-locked path.
    pub(super) fn with_current<R>(
        &self,
        valid: impl FnOnce() -> bool,
        publish: impl FnOnce(&mut ShardBuffer<'_>) -> R,
    ) -> Option<R> {
        let mut entries = self.shards[SHARD.with(|shard| *shard)].0.lock().unwrap();
        if self.sealed.load(Ordering::Acquire) || !valid() {
            return None;
        }
        Some(publish(&mut ShardBuffer {
            capacity: self.capacity,
            entries: &mut entries,
        }))
    }

    /// Stop shard publication and hand every pending entry to `publish`.
    /// Idempotent; later producers fall back to the heap-locked path.
    pub(super) fn seal(&self, mut publish: impl FnMut(&[usize])) {
        self.sealed.store(true, Ordering::SeqCst);
        for shard in &self.shards {
            let mut entries = shard.0.lock().unwrap();
            if !entries.is_empty() {
                publish(&entries);
                entries.clear();
            }
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.shards
            .iter()
            .all(|shard| shard.0.lock().unwrap().is_empty())
    }
}

pub(super) struct ShardBuffer<'a> {
    capacity: usize,
    entries: &'a mut Vec<usize>,
}

impl ShardBuffer<'_> {
    pub(super) fn record(&mut self, value: usize, publish: impl FnOnce(&[usize])) {
        if value == 0 {
            return;
        }
        if self.entries.capacity() == 0 {
            self.entries.reserve_exact(self.capacity);
        }
        self.entries.push(value);
        if self.entries.len() >= self.capacity {
            publish(self.entries);
            self.entries.clear();
        }
    }

    pub(super) fn flush(&mut self, publish: impl FnOnce(&[usize])) {
        if !self.entries.is_empty() {
            publish(self.entries);
            self.entries.clear();
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
    fn shards_publish_full_buffers_and_seal_once() {
        for capacity in [1, 3, 256] {
            let shards = SatbShards::new(capacity);
            let mut published = Vec::new();
            for value in 1..=1000 {
                shards
                    .with_current(
                        || true,
                        |buffer| buffer.record(value, |v| published.extend_from_slice(v)),
                    )
                    .expect("unsealed shard accepts entries");
            }
            assert_eq!(published.len(), 1000 / capacity * capacity);
            assert!(shards.with_current(|| false, |_| ()).is_none());
            shards.seal(|v| published.extend_from_slice(v));
            assert_eq!(published, (1..=1000).collect::<Vec<_>>());
            assert!(shards.is_empty());
            assert!(shards.with_current(|| true, |_| ()).is_none());
            shards.seal(|_| panic!("a sealed shard republished"));
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
