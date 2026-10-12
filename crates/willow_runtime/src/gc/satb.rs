//! Per-mutator deletion buffers. The owning heap mutex serializes producers
//! with STW drains; callbacks publish values only and never trace/allocate GC.
//! `SatbShards` are per-cycle buffers that concurrent-mark barriers fill under
//! a private buffer mutex instead of the heap mutex (willow-jz15.53).
//! The cycle's sharded registry retains buffers while publication is possible.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

fn configured_capacity() -> usize {
    capacity_from_setting(
        std::env::var("WILLOW_GC_SATB_BUFFER_ENTRIES")
            .ok()
            .as_deref(),
    )
}

fn capacity_from_setting(setting: Option<&str>) -> usize {
    setting
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
        // A previous publication may have unwound with a full buffer. Drain
        // that buffer before appending so recovery never grows past capacity.
        if entries.len() >= self.capacity {
            publish(entries);
            entries.clear();
        }
        entries.push(value);
        if entries.len() >= self.capacity {
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

/// Fixed registry shard count. Threads share only registry locks, never entry
/// storage: each mutator has its own fixed-capacity buffer. Memory is O(M * B).
const SATB_SHARDS: usize = 64;

thread_local! {
    static SHARD: usize = {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed) % SATB_SHARDS
    };
}

/// Per-cycle deletion buffers published without the heap mutex.
///
/// A producer obtains its private buffer from a sharded registry, releases
/// the registry lock, then checks `sealed` and epoch validity under the buffer
/// lock. Publication completes under the buffer lock. `seal` first publishes
/// `sealed`, then drains each buffer under that same lock. Thus publication
/// precedes its drain or observes sealing and falls back to the heap lock.
/// Lock order: heap -> registry -> private buffer -> mark queue; producers
/// release the registry lock before locking their private buffer.
pub(super) struct SatbShards {
    capacity: usize,
    sealed: AtomicBool,
    drained: AtomicBool,
    #[cfg(test)]
    drain_visits: AtomicUsize,
    shards: Box<[Shard]>,
}

/// Padded so neighbouring threads' shard locks do not share a cache line.
#[repr(align(128))]
struct Shard(Mutex<HashMap<ThreadId, Arc<MutatorBuffer>>>);

/// The registration record owns the buffer; a cycle retains an Arc until its
/// final drain. A mutex publishes entries even during concurrent closure.
pub(super) struct MutatorBuffer(Mutex<Vec<usize>>);

impl Default for MutatorBuffer {
    fn default() -> Self {
        Self(Mutex::new(Vec::with_capacity(configured_capacity())))
    }
}

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
            drained: AtomicBool::new(false),
            #[cfg(test)]
            drain_visits: AtomicUsize::new(0),
            shards: (0..SATB_SHARDS)
                .map(|_| Shard(Mutex::new(HashMap::new())))
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
        let buffer = {
            let mut buffers = self.shards[SHARD.with(|shard| *shard)]
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.sealed.load(Ordering::Acquire) {
                return None;
            }
            Arc::clone(
                buffers
                    .entry(std::thread::current().id())
                    .or_insert_with(|| {
                        let buffer = super::safepoint::current_satb_buffer().unwrap_or_default();
                        assert!(
                            buffer
                                .0
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .is_empty(),
                            "initial epoch registration found undrained SATB entries"
                        );
                        buffer
                    }),
            )
        };
        let mut entries = buffer.0.lock().unwrap_or_else(|error| error.into_inner());
        if self.sealed.load(Ordering::Acquire) || !valid() {
            return None;
        }
        Some(publish(&mut ShardBuffer {
            capacity: self.capacity,
            entries: &mut entries,
        }))
    }

    pub(super) fn retire_current(&self) {
        let mut buffers = self.shards[SHARD.with(|shard| *shard)]
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(buffer) = buffers.remove(&std::thread::current().id()) {
            assert!(
                buffer
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_empty(),
                "retirement must drain first"
            );
        }
    }

    pub(super) fn seal_without_drain(&self) {
        // Finishing paths drain under COORD/STW first. This atomic revocation
        // must not inspect foreign storage after mutators resume.
        self.sealed.store(true, Ordering::Release);
    }

    /// Stop shard publication and hand every pending entry to `publish`.
    /// Idempotent; later producers fall back to the heap-locked path.
    pub(super) fn seal_stopped(&self, coord: &super::GcCoord, publish: impl FnMut(&[usize])) {
        assert!(coord.stop_requested);
        let current = std::thread::current().id();
        assert!(
            coord
                .mutators
                .keys()
                .all(|id| *id == current || coord.parked.contains(id))
        );
        self.seal(publish);
    }

    fn seal(&self, mut publish: impl FnMut(&[usize])) {
        if self.drained.load(Ordering::Acquire) {
            return;
        }
        self.sealed.store(true, Ordering::SeqCst);
        for shard in &self.shards {
            let buffers = shard.0.lock().unwrap_or_else(|error| error.into_inner());
            for buffer in buffers.values() {
                #[cfg(test)]
                self.drain_visits.fetch_add(1, Ordering::Relaxed);
                let mut entries = buffer.0.lock().unwrap_or_else(|error| error.into_inner());
                if !entries.is_empty() {
                    publish(&entries);
                    entries.clear();
                }
            }
        }
        self.drained.store(true, Ordering::Release);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.shards.iter().all(|shard| {
            shard.0.lock().unwrap().values().all(|buffer| {
                buffer
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_empty()
            })
        })
    }
}

pub(super) struct ShardBuffer<'a> {
    capacity: usize,
    entries: &'a mut Vec<usize>,
}

impl ShardBuffer<'_> {
    pub(super) fn record(&mut self, value: usize, mut publish: impl FnMut(&[usize])) {
        if value == 0 {
            return;
        }
        if self.entries.capacity() == 0 {
            self.entries.reserve_exact(self.capacity);
        }
        if self.entries.len() >= self.capacity {
            publish(self.entries);
            self.entries.clear();
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
    fn foreign_drain_rejects_running_owners_and_accepts_parked_or_unregistered() {
        let foreign = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        let mut coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        coord.mutators.insert(foreign, Default::default());
        let buffers = SatbShards::new(256);
        let mut published = Vec::new();
        buffers
            .with_current(|| true, |buffer| buffer.record(7, |_| panic!("not full")))
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                buffers.seal_stopped(&coord, |_| panic!("must reject before touching entries"));
            }))
            .is_err()
        );
        assert!(!buffers.sealed.load(Ordering::Acquire));
        coord.parked.insert(foreign);
        buffers.seal_stopped(&coord, |entries| published.extend_from_slice(entries));
        assert_eq!(published, vec![7]);
        let buffers = SatbShards::new(256);
        coord.parked.remove(&foreign);
        coord.mutators.remove(&foreign);
        buffers.seal_stopped(&coord, |_| panic!("empty"));
    }

    #[test]
    fn capacity_setting_accepts_one_and_rejects_out_of_range_values() {
        for (setting, expected) in [
            (None, 256),
            (Some("1"), 1),
            (Some("65536"), 65536),
            (Some("0"), 256),
            (Some("65537"), 256),
            (Some("-1"), 256),
            (Some("invalid"), 256),
        ] {
            assert_eq!(capacity_from_setting(setting), expected);
        }
    }

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
            let visits = shards.drain_visits.load(Ordering::Relaxed);
            assert_eq!(visits, 1);
            shards.seal(|_| panic!("a sealed shard republished"));
            assert_eq!(shards.drain_visits.load(Ordering::Relaxed), visits);
        }
    }

    #[test]
    fn more_mutators_than_registry_shards_keep_independent_buffers() {
        for (capacity, count) in [1, 256]
            .into_iter()
            .flat_map(|capacity| [1, 16, 80].map(|count| (capacity, count)))
        {
            let buffers = Arc::new(SatbShards::new(capacity));
            let published = Arc::new(Mutex::new(Vec::new()));
            let ready = Arc::new(std::sync::Barrier::new(count + 1));
            let finish = Arc::new(std::sync::Barrier::new(count + 1));
            std::thread::scope(|scope| {
                for value in 1..=count {
                    let (buffers, published, ready, finish) = (
                        buffers.clone(),
                        published.clone(),
                        ready.clone(),
                        finish.clone(),
                    );
                    scope.spawn(move || {
                        buffers
                            .with_current(
                                || true,
                                |buffer| {
                                    buffer.record(value, |values| {
                                        published.lock().unwrap().extend_from_slice(values)
                                    });
                                },
                            )
                            .unwrap();
                        ready.wait();
                        finish.wait();
                        buffers
                            .with_current(
                                || true,
                                |buffer| {
                                    assert_eq!(
                                        buffer.entries.as_slice(),
                                        if capacity == 1 {
                                            &[][..]
                                        } else {
                                            std::slice::from_ref(&value)
                                        }
                                    );
                                    buffer.flush(|values| {
                                        published.lock().unwrap().extend_from_slice(values)
                                    });
                                },
                            )
                            .unwrap();
                        buffers.retire_current();
                    });
                }
                ready.wait();
                let registered_count: usize = buffers
                    .shards
                    .iter()
                    .map(|shard| shard.0.lock().unwrap().len())
                    .sum();
                assert_eq!(registered_count, count);
                finish.wait();
            });
            buffers.seal(|_| panic!("retired buffers must be empty"));
            let mut values = published.lock().unwrap().clone();
            values.sort_unstable();
            println!(
                "satb mutators={count} capacity={capacity} buffers={count} published={}",
                values.len()
            );
            assert_eq!(values, (1..=count).collect::<Vec<_>>());
            assert!(
                buffers
                    .shards
                    .iter()
                    .all(|shard| shard.0.lock().unwrap().is_empty())
            );
        }
    }

    #[test]
    fn publication_panic_keeps_entries_for_the_final_drain() {
        let buffers = SatbShards::new(1);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                buffers.with_current(
                    || true,
                    |buffer| buffer.record(7, |_| panic!("publication interrupted")),
                );
            }))
            .is_err()
        );
        let mut values = Vec::new();
        buffers
            .with_current(
                || true,
                |buffer| {
                    buffer.record(8, |entries| {
                        assert!(entries.len() <= 1);
                        values.extend_from_slice(entries);
                    });
                },
            )
            .unwrap();
        buffers.seal(|entries| values.extend_from_slice(entries));
        assert_eq!(values, vec![7, 8]);
        assert!(buffers.is_empty());
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
