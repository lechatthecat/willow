//! Per-mutator deletion buffers. The owning heap mutex serializes producers
//! with STW drains; callbacks publish values only and never trace/allocate GC.
//! `SatbShards` are per-cycle buffers that concurrent-mark barriers fill under
//! a private buffer mutex instead of the heap mutex (willow-jz15.53).
//! The cycle's sharded registry retains buffers while publication is possible.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

fn assert_stopped_owners(coord: &super::GcCoord) {
    assert!(
        coord.stop_requested,
        "foreign SATB inspection requires STW under COORD"
    );
    // with_stw builds pending before its rendezvous and removes owners as
    // they park/unregister. It calls the collector only once pending is empty,
    // retaining COORD so registration/resume cannot invalidate this proof.
    assert!(
        coord.stw_pending.is_empty(),
        "foreign SATB owner must park or unregister before inspection"
    );
}

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

#[cfg(test)]
std::thread_local! {
    static ENTRY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LEGACY_EMPTY_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Successfully published prefixes remain in place until the buffer empties.
/// This keeps partial transfer O(entries) cumulatively, without shifting tails.
#[repr(C)]
struct EntryBuffer {
    base: std::cell::Cell<usize>,
    length: std::cell::Cell<usize>,
    inline_limit: std::cell::Cell<usize>,
    closing: std::cell::Cell<usize>,
    values: Vec<usize>,
    published: usize,
}
impl std::ops::Deref for EntryBuffer {
    type Target = [usize];
    fn deref(&self) -> &[usize] {
        #[cfg(test)]
        ENTRY_READS.set(ENTRY_READS.get() + 1);
        // Only the owner can append without this buffer's mutex. Foreign reads
        // require stopped ownership; the Vec allocation is stable while admitted.
        unsafe { std::slice::from_raw_parts(self.values.as_ptr(), self.length.get()) }
            .get(self.published..)
            .expect("published SATB prefix")
    }
}
impl Default for EntryBuffer {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}
const _: () = {
    use willow_abi::barrier_buffer as abi;
    assert!(std::mem::offset_of!(EntryBuffer, base) == abi::BASE_OFFSET as usize);
    assert!(std::mem::offset_of!(EntryBuffer, length) == abi::LENGTH_OFFSET as usize);
    assert!(std::mem::offset_of!(EntryBuffer, inline_limit) == abi::LIMIT_OFFSET as usize);
    assert!(std::mem::offset_of!(EntryBuffer, closing) == abi::CLOSING_OFFSET as usize);
};
impl EntryBuffer {
    fn with_capacity(capacity: usize) -> Self {
        let values = Vec::with_capacity(capacity);
        Self {
            base: std::cell::Cell::new(values.as_ptr() as usize),
            length: std::cell::Cell::new(0),
            inline_limit: std::cell::Cell::new(0),
            closing: std::cell::Cell::new(0),
            values,
            published: 0,
        }
    }
    fn capacity(&self) -> usize {
        self.values.capacity()
    }
    fn sync_length(&mut self) {
        assert!(self.length.get() <= self.values.capacity());
        // Generated stores initialize every word before publishing length.
        unsafe {
            self.values.set_len(self.length.get());
        }
    }
    fn reserve_exact(&mut self, capacity: usize) {
        self.sync_length();
        self.values.reserve_exact(capacity);
        self.base.set(self.values.as_ptr() as usize);
    }
    fn push(&mut self, value: usize) {
        self.sync_length();
        self.values.push(value);
        self.base.set(self.values.as_ptr() as usize);
        self.length.set(self.values.len());
    }
    fn clear(&mut self) {
        self.values.clear();
        self.length.set(0);
        self.published = 0;
    }
}

fn transfer_bounded(
    entries: &mut EntryBuffer,
    budget: &mut super::old_cycle::RemarkBudget,
    now: &mut impl FnMut() -> u64,
    publish: &mut impl FnMut(&[usize]),
) -> bool {
    // Matches enqueue_satb_batch's queue-item bound: a callback processes at
    // most one chunk before the next deadline/work admission check.
    const CHUNK: usize = 32;
    while !entries.is_empty() {
        let count = budget.entry_capacity(
            entries.len().min(CHUNK),
            std::mem::size_of::<usize>() as u64,
        );
        if count == 0 {
            // Account the rejected smallest unit, retaining the untouched tail.
            budget.reserve(1, std::mem::size_of::<usize>() as u64, now());
            return false;
        }
        if !budget.reserve(
            count as u64,
            (count * std::mem::size_of::<usize>()) as u64,
            now(),
        ) {
            return false;
        }
        publish(&entries[..count]);
        entries.published += count;
    }
    entries.clear();
    budget.reserve(0, 0, now())
}

/// Update logical pending-entry accounting even when publication unwinds or
/// a bounded transfer returns with a partially published prefix.
fn account_pending<R>(
    entries: &mut EntryBuffer,
    pending: &mut usize,
    operation: impl FnOnce(&mut EntryBuffer) -> R,
) -> R {
    struct Accounting<'a> {
        entries: &'a mut EntryBuffer,
        pending: &'a mut usize,
        before: usize,
    }
    impl Drop for Accounting<'_> {
        fn drop(&mut self) {
            *self.pending = *self.pending - self.before + self.entries.len();
        }
    }
    let before = entries.len();
    let guard = Accounting {
        entries,
        pending,
        before,
    };
    operation(&mut *guard.entries)
}

pub(super) struct SatbBuffers {
    capacity: usize,
    entries: HashMap<ThreadId, EntryBuffer>,
    pending_entries: usize,
    pending_owners: super::pending_registry::Pending<ThreadId>,
}

impl Default for SatbBuffers {
    fn default() -> Self {
        Self::new(configured_capacity())
    }
}

impl SatbBuffers {
    pub(super) fn is_empty(&self) -> bool {
        #[cfg(test)]
        LEGACY_EMPTY_CHECKS.set(LEGACY_EMPTY_CHECKS.get() + 1);
        self.pending_entries == 0
    }

    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            entries: HashMap::new(),
            pending_entries: 0,
            pending_owners: Default::default(),
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
        // Enrol before publication: an unwinding callback retains ownership.
        self.pending_owners.insert(thread);
        let entries = self
            .entries
            .entry(thread)
            .or_insert_with(|| EntryBuffer::with_capacity(self.capacity));
        account_pending(entries, &mut self.pending_entries, |entries| {
            // A failed publication leaves the full buffer owned for retry.
            if entries.len() >= self.capacity {
                publish(entries);
                entries.clear();
            }
            entries.push(value);
            if entries.len() >= self.capacity {
                publish(entries);
                entries.clear();
            }
        });
        if entries.is_empty() {
            self.pending_owners.remove(thread);
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
            account_pending(entries, &mut self.pending_entries, |entries| {
                publish(entries);
                entries.clear();
            });
        }
        self.pending_owners.remove(thread);
        if retire {
            self.entries.remove(&thread);
        }
    }

    pub(super) fn flush_all_bounded(
        &mut self,
        coord: &super::GcCoord,
        budget: &mut super::old_cycle::RemarkBudget,
        mut publish: impl FnMut(&[usize]),
    ) -> bool {
        self.flush_all_bounded_with_clock(
            coord,
            budget,
            crate::gc_telemetry::timestamp_ns,
            &mut publish,
        )
    }

    fn flush_all_bounded_with_clock(
        &mut self,
        coord: &super::GcCoord,
        budget: &mut super::old_cycle::RemarkBudget,
        mut now: impl FnMut() -> u64,
        mut publish: impl FnMut(&[usize]),
    ) -> bool {
        if !budget.reserve(1, 0, now()) {
            return false;
        }
        assert_stopped_owners(coord);
        while let Some(owner) = self.pending_owners.front() {
            if !budget.reserve(1, 0, now()) {
                return false;
            }
            let entries = self.entries.get_mut(&owner).expect("pending SATB owner");
            if !account_pending(entries, &mut self.pending_entries, |entries| {
                transfer_bounded(entries, budget, &mut now, &mut publish)
            }) {
                return false;
            }
            self.pending_owners.remove(owner);
        }
        true
    }

    pub(super) fn flush_all(&mut self, mut publish: impl FnMut(&[usize])) {
        while let Some(owner) = self.pending_owners.front() {
            let entries = self.entries.get_mut(&owner).expect("pending SATB owner");
            if !entries.is_empty() {
                account_pending(entries, &mut self.pending_entries, |entries| {
                    publish(entries);
                    entries.clear();
                });
            }
            self.pending_owners.remove(owner);
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
/// lock. Publication completes under the buffer lock. Stopped sealing validates
/// ownership under COORD before inspecting foreign buffers, then publishes
/// `sealed` and drains under the buffer lock. Thus publication
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
    seal_cursor: Mutex<(usize, bool)>,
}

/// Padded so neighbouring threads' shard locks do not share a cache line.
#[repr(align(128))]
struct Shard(Mutex<BufferRegistry>);

#[derive(Default)]
struct BufferRegistry {
    entries: HashMap<ThreadId, Arc<MutatorBuffer>>,
    owners: super::pending_registry::Pending<ThreadId>,
}
impl BufferRegistry {
    fn insert(&mut self, owner: ThreadId, buffer: Arc<MutatorBuffer>) {
        self.owners.insert(owner);
        self.entries.insert(owner, buffer);
    }
    fn remove(&mut self, owner: &ThreadId) {
        self.owners.remove(*owner);
        self.entries.remove(owner);
    }
    fn get(&self, owner: &ThreadId) -> Option<&Arc<MutatorBuffer>> {
        self.entries.get(owner)
    }
    #[cfg(test)]
    fn values(&self) -> impl Iterator<Item = &Arc<MutatorBuffer>> {
        self.entries.values()
    }
}

/// The registration record owns the buffer; a cycle retains an Arc until its
/// final stopped drain. Only the owner publishes while running; foreign reads
/// additionally require stopped ownership under COORD.
pub(super) struct MutatorBuffer(Mutex<EntryBuffer>);

impl Default for MutatorBuffer {
    fn default() -> Self {
        Self(Mutex::new(
            EntryBuffer::with_capacity(configured_capacity()),
        ))
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
                .map(|_| Shard(Mutex::new(BufferRegistry::default())))
                .collect(),
            seal_cursor: Mutex::new((0, false)),
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
            let mut buffers = crate::gc_telemetry::m3::lock(
                &self.shards[SHARD.with(|shard| *shard)].0,
                crate::gc_telemetry::m3::Kind::SatbRegistry,
            );
            if self.sealed.load(Ordering::Acquire) {
                return None;
            }
            let owner = std::thread::current().id();
            if buffers.get(&owner).is_none() {
                let buffer = super::safepoint::current_satb_buffer().unwrap_or_default();
                assert!(
                    buffer
                        .0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty(),
                    "initial epoch registration found undrained SATB entries"
                );
                buffers.insert(owner, buffer);
            }
            Arc::clone(buffers.get(&owner).unwrap())
        };
        let mut entries =
            crate::gc_telemetry::m3::lock(&buffer.0, crate::gc_telemetry::m3::Kind::SatbBuffer);
        if self.sealed.load(Ordering::Acquire) || !valid() {
            return None;
        }
        Some(publish(&mut ShardBuffer {
            capacity: self.capacity,
            entries: &mut entries,
        }))
    }

    pub(super) fn retire_current(&self, mut publish: impl FnMut(&[usize])) {
        let mut buffers = self.shards[SHARD.with(|shard| *shard)]
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let owner = std::thread::current().id();
        if let Some(buffer) = buffers.get(&owner).cloned() {
            let mut entries =
                crate::gc_telemetry::m3::lock(&buffer.0, crate::gc_telemetry::m3::Kind::SatbBuffer);
            entries.inline_limit.set(0);
            if !entries.is_empty() {
                // Closure may have revoked private publication before this
                // owner reaches unregister. Transfer before removing storage.
                publish(&entries);
                entries.clear();
            }
            drop(entries);
            // Removal follows successful publication, so an unwinding callback
            // leaves owned entries reachable by authoritative seal/recovery.
            buffers.remove(&owner);
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
        assert_stopped_owners(coord);
        self.seal(publish);
    }

    /// Only a stopped ownership boundary may inspect foreign buffers. Reserve
    /// registry and entry work before transfer; rejected buffers keep all entries
    /// reachable for authoritative Recovery, while admitted buffers clear once.
    pub(super) fn seal_stopped_bounded(
        &self,
        coord: &super::GcCoord,
        budget: &mut super::old_cycle::RemarkBudget,
        mut publish: impl FnMut(&[usize]),
    ) -> bool {
        let _profile = crate::gc_telemetry::m3::Span::new(crate::gc_telemetry::m3::Kind::SatbSeal);
        self.seal_stopped_bounded_with_clock(
            coord,
            budget,
            crate::gc_telemetry::timestamp_ns,
            &mut publish,
        )
    }

    fn seal_stopped_bounded_with_clock(
        &self,
        coord: &super::GcCoord,
        budget: &mut super::old_cycle::RemarkBudget,
        mut now: impl FnMut() -> u64,
        mut publish: impl FnMut(&[usize]),
    ) -> bool {
        if !budget.reserve(1, 0, now()) {
            return false;
        }
        assert_stopped_owners(coord);
        self.sealed.store(true, Ordering::SeqCst);
        if self.drained.load(Ordering::Acquire) {
            return true;
        }
        let mut cursor = self.seal_cursor.lock().unwrap_or_else(|e| e.into_inner());
        while cursor.0 < self.shards.len() {
            if !cursor.1 {
                if !budget.reserve(1, 0, now()) {
                    return false;
                }
                cursor.1 = true;
            }
            let mut buffers = self.shards[cursor.0]
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            while let Some(owner) = buffers.owners.front() {
                if !budget.reserve(1, 0, now()) {
                    return false;
                }
                #[cfg(test)]
                self.drain_visits.fetch_add(1, Ordering::Relaxed);
                let mut entries = buffers
                    .get(&owner)
                    .unwrap()
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                entries.inline_limit.set(0);
                if !transfer_bounded(&mut entries, budget, &mut now, &mut publish) {
                    return false;
                }
                drop(entries);
                buffers.remove(&owner);
            }
            cursor.0 += 1;
            cursor.1 = false;
        }
        self.drained.store(true, Ordering::Release);
        true
    }

    /// Minor relocation has translated the injector and every buffer is empty.
    /// Resume private publication in the same epoch, while all owners remain
    /// stopped. Closure must keep its authoritative seal instead.
    pub(super) fn reopen_stopped(&self, coord: &super::GcCoord) {
        assert!(coord.stop_requested);
        let current = std::thread::current().id();
        assert!(
            coord
                .mutators
                .keys()
                .all(|id| *id == current || coord.parked.contains(id))
        );
        assert!(self.drained.load(Ordering::Acquire));
        *self.seal_cursor.lock().unwrap_or_else(|e| e.into_inner()) = (0, false);
        self.drained.store(false, Ordering::Release);
        self.sealed.store(false, Ordering::Release);
    }

    fn seal(&self, mut publish: impl FnMut(&[usize])) {
        if self.drained.load(Ordering::Acquire) {
            return;
        }
        self.sealed.store(true, Ordering::SeqCst);
        let mut cursor = self.seal_cursor.lock().unwrap_or_else(|e| e.into_inner());
        while cursor.0 < self.shards.len() {
            let mut buffers = self.shards[cursor.0]
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            while let Some(owner) = buffers.owners.front() {
                #[cfg(test)]
                self.drain_visits.fetch_add(1, Ordering::Relaxed);
                let mut entries = buffers
                    .get(&owner)
                    .unwrap()
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                entries.inline_limit.set(0);
                if !entries.is_empty() {
                    publish(&entries);
                    entries.clear();
                }
                drop(entries);
                buffers.remove(&owner);
            }
            cursor.0 += 1;
            cursor.1 = false;
        }
        self.drained.store(true, Ordering::Release);
    }

    /// Constant-time proof that the one authoritative stopped seal finished.
    pub(super) fn is_drained(&self) -> bool {
        self.drained.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.shards.iter().all(|shard| {
            shard
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .all(|buffer| {
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
    entries: &'a mut EntryBuffer,
}

impl ShardBuffer<'_> {
    pub(super) fn admit_inline(&mut self, closing: &AtomicBool) -> usize {
        self.entries
            .closing
            .set(closing as *const AtomicBool as usize);
        assert_eq!(self.entries.published, 0);
        self.entries
            .inline_limit
            .set(self.capacity.min(self.entries.capacity()));
        self.entries as *mut EntryBuffer as usize
    }

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

    static OPEN_EPOCH: AtomicBool = AtomicBool::new(false);

    // Execute the same ABI accesses as generated code, with no Rust Vec borrow.
    unsafe fn inline_pair(address: usize, pair: [usize; 2]) -> bool {
        use willow_abi::barrier_buffer as abi;
        unsafe {
            let field = |offset: i32| (address + offset as usize) as *mut usize;
            let length = field(abi::LENGTH_OFFSET).read();
            let limit = field(abi::LIMIT_OFFSET).read();
            if length + 2 > limit {
                return false;
            }
            let closing = field(abi::CLOSING_OFFSET).read() as *const AtomicBool;
            if (*closing).load(Ordering::Acquire) {
                return false;
            }
            let base = field(abi::BASE_OFFSET).read() as *mut usize;
            base.add(length).write(pair[0]);
            base.add(length + 1).write(pair[1]);
            field(abi::LENGTH_OFFSET).write(length + 2);
        }
        true
    }

    #[test]
    fn inline_satb_pairs_preserve_capacity_order_and_runtime_interoperation() {
        for capacity in [1, 2, 3, 32, 256] {
            for pairs in [1, 16, 256] {
                let buffers = SatbShards::new(capacity);
                let mut published = Vec::new();
                let mut expected = Vec::new();
                for i in 0..pairs {
                    let address = buffers
                        .with_current(|| true, |buffer| buffer.admit_inline(&OPEN_EPOCH))
                        .unwrap();
                    let pair = [i * 2 + 1, i * 2 + 2];
                    if !unsafe { inline_pair(address, pair) } {
                        buffers
                            .with_current(
                                || true,
                                |buffer| {
                                    for value in pair {
                                        buffer.record(value, |v| published.extend_from_slice(v));
                                    }
                                },
                            )
                            .unwrap();
                    }
                    expected.extend(pair);
                }
                buffers.retire_current(|v| published.extend_from_slice(v));
                assert_eq!(published, expected, "capacity={capacity} pairs={pairs}");
                assert!(buffers.is_empty());
            }
        }
    }

    #[test]
    fn inline_satb_closing_revokes_append_before_stopped_seal() {
        let closing = AtomicBool::new(false);
        let buffers = SatbShards::new(32);
        let address = buffers
            .with_current(|| true, |buffer| buffer.admit_inline(&closing))
            .unwrap();
        assert!(unsafe { inline_pair(address, [7, 9]) });
        closing.store(true, Ordering::Release);
        assert!(!unsafe { inline_pair(address, [11, 13]) });
        let coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        let mut values = Vec::new();
        buffers.seal_stopped(&coord, |v| values.extend_from_slice(v));
        assert_eq!(values, [7, 9]);
    }

    #[test]
    fn inline_satb_bounded_seal_preserves_suffix_and_revokes_reused_storage() {
        let _guard = super::super::runtime_test_guard();
        super::super::reset_internal_for_test();
        super::super::willow_gc_register_mutator();
        let buffers = SatbShards::new(256);
        let address = buffers
            .with_current(|| true, |buffer| buffer.admit_inline(&OPEN_EPOCH))
            .unwrap();
        for i in 0..64 {
            assert!(unsafe { inline_pair(address, [i * 2, i * 2 + 1]) });
        }
        let coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        let mut published = Vec::new();
        let mut rounds = 0;
        loop {
            let mut budget = super::super::old_cycle::RemarkBudget::new(4096, 8, u64::MAX, 0);
            rounds += 1;
            if buffers.seal_stopped_bounded_with_clock(
                &coord,
                &mut budget,
                || 0,
                |v| published.extend_from_slice(v),
            ) {
                break;
            }
            assert!(rounds < 200);
            assert!(!unsafe { inline_pair(address, [999, 999]) });
        }
        assert_eq!(published, (0..128).collect::<Vec<_>>());
        assert!(!unsafe { inline_pair(address, [999, 999]) });
        buffers.reopen_stopped(&coord);
        assert!(!unsafe { inline_pair(address, [999, 999]) });
        let readmitted = buffers
            .with_current(|| true, |buffer| buffer.admit_inline(&OPEN_EPOCH))
            .unwrap();
        assert_eq!(address, readmitted);
        assert!(unsafe { inline_pair(address, [129, 130]) });
        buffers.retire_current(|v| published.extend_from_slice(v));
        assert_eq!(&published[128..], &[129, 130]);
        assert!(!unsafe { inline_pair(address, [999, 999]) });
        super::super::willow_gc_unregister_mutator();
        super::super::reset_internal_for_test();
    }

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
        coord.stw_pending.insert(foreign);
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
        coord.stw_pending.remove(&foreign);
        buffers.seal_stopped(&coord, |entries| published.extend_from_slice(entries));
        assert_eq!(published, vec![7]);
        let buffers = SatbShards::new(256);
        coord.parked.remove(&foreign);
        coord.mutators.remove(&foreign);
        buffers.seal_stopped(&coord, |_| panic!("empty"));
    }

    #[test]
    fn bounded_stopped_satb_transfer_rejects_running_owner_and_retains_unadmitted_entries() {
        let buffers = Arc::new(SatbShards::new(256));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let owner_buffers = buffers.clone();
        let owner = std::thread::spawn(move || {
            owner_buffers
                .with_current(
                    || true,
                    |buffer| {
                        buffer.record(7, |_| unreachable!());
                        buffer.record(9, |_| unreachable!());
                    },
                )
                .unwrap();
            ready_tx.send(std::thread::current().id()).unwrap();
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(5));
        });
        let foreign = ready_rx.recv().unwrap();
        let mut coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        coord.mutators.insert(foreign, Default::default());
        coord.stw_pending.insert(foreign);
        let mut budget = super::super::old_cycle::RemarkBudget::new(4096, 8, u64::MAX, 0);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                buffers.seal_stopped_bounded(&coord, &mut budget, |_| {
                    panic!("running owner must not be inspected")
                });
            }))
            .is_err()
        );
        assert!(!buffers.sealed.load(Ordering::Acquire));
        // The owner is waiting without accessing its buffer. Marking it parked
        // models the COORD-owned stopped snapshot; no foreign read precedes it.
        coord.parked.insert(foreign);
        coord.stw_pending.remove(&foreign);
        let mut published = Vec::new();
        assert!(
            !buffers.seal_stopped_bounded(&coord, &mut budget, |values| published
                .extend_from_slice(values))
        );
        assert_eq!(published, [7], "only the admitted entry is published");
        assert!(!buffers.is_drained());
        buffers.seal_stopped(&coord, |values| published.extend_from_slice(values));
        assert_eq!(published, [7, 9]);
        buffers.seal_stopped(&coord, |_| panic!("must not republish"));
        assert!(buffers.is_drained());
        done_tx.send(()).unwrap();
        owner.join().unwrap();
    }

    #[test]
    fn satb_both_buffers_fake_clock_preserves_mid_publication_suffix_exactly_once() {
        for count in [65, 1025, 65535] {
            for sharded in [false, true] {
                let coord = super::super::GcCoord {
                    stop_requested: true,
                    ..Default::default()
                };
                let clock = std::cell::Cell::new(0);
                let mut budget =
                    super::super::old_cycle::RemarkBudget::new(1000000, 1048576, 10, 0);
                let mut published = Vec::new();
                let mut legacy = SatbBuffers::new(65536);
                let shards = SatbShards::new(65536);
                for value in 1..=count {
                    if sharded {
                        shards
                            .with_current(|| true, |b| b.record(value, |_| panic!("not full")))
                            .unwrap();
                    } else {
                        legacy.record(std::thread::current().id(), value, |_| panic!("not full"));
                    }
                }
                let mut publish = |values: &[usize]| {
                    assert!(values.len() <= 32);
                    published.extend_from_slice(values);
                    clock.set(10); // Deadline expires while the first callback runs.
                };
                let finished = if sharded {
                    shards.seal_stopped_bounded_with_clock(
                        &coord,
                        &mut budget,
                        || clock.get(),
                        &mut publish,
                    )
                } else {
                    legacy.flush_all_bounded_with_clock(
                        &coord,
                        &mut budget,
                        || clock.get(),
                        &mut publish,
                    )
                };
                assert!(!finished);
                assert_eq!(published, (1..=32).collect::<Vec<_>>());
                let mut budget =
                    super::super::old_cycle::RemarkBudget::new(1000000, 1048576, 10, 0);
                if sharded {
                    assert!(!shards.is_drained());
                    assert!(shards.seal_stopped_bounded_with_clock(
                        &coord,
                        &mut budget,
                        || 0,
                        |v| published.extend_from_slice(v)
                    ));
                    shards.seal_stopped(&coord, |_| panic!("duplicate"));
                } else {
                    assert!(legacy.flush_all_bounded_with_clock(
                        &coord,
                        &mut budget,
                        || 0,
                        |v| published.extend_from_slice(v)
                    ));
                    legacy.flush_all(|_| panic!("duplicate"));
                }
                assert_eq!(published, (1..=count).collect::<Vec<_>>());
                println!(
                    "satb sharded={sharded} entries={count} first_transfer=32 suffix={} total_transferred={} prefix_moves=0",
                    count - 32,
                    published.len()
                );
            }
        }
    }

    #[test]
    fn satb_registry_resume_scales_with_pending_owners_not_retry_prefixes() {
        let ids: Vec<_> = (0..257)
            .map(|_| {
                std::thread::spawn(|| std::thread::current().id())
                    .join()
                    .unwrap()
            })
            .collect();
        let coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        for count in [1, 16, 257] {
            for sharded in [false, true] {
                let mut legacy = SatbBuffers::new(256);
                let shards = SatbShards::new(256);
                for (value, &id) in ids[..count].iter().enumerate() {
                    legacy.record(id, value + 1, |_| unreachable!());
                    let buffer = Arc::new(MutatorBuffer::default());
                    buffer.0.lock().unwrap().push(value + 1);
                    // All owners collide in one shard: retries must resume inside it.
                    shards.shards[0].0.lock().unwrap().insert(id, buffer);
                }
                let mut delivered = Vec::new();
                let checks = std::cell::Cell::new(0);
                let mut rounds = 0;
                loop {
                    rounds += 1;
                    assert!(
                        rounds <= 2 * count + SATB_SHARDS + 3,
                        "registry did not progress"
                    );
                    let mut budget = super::super::old_cycle::RemarkBudget::new(4, 8, 10, 0);
                    let now = || {
                        checks.set(checks.get() + 1);
                        0
                    };
                    let before = delivered.len();
                    let done = if sharded {
                        shards.seal_stopped_bounded_with_clock(&coord, &mut budget, now, |v| {
                            delivered.extend_from_slice(v)
                        })
                    } else {
                        legacy.flush_all_bounded_with_clock(&coord, &mut budget, now, |v| {
                            delivered.extend_from_slice(v)
                        })
                    };
                    assert!(
                        delivered.len() - before <= 1,
                        "per-stop byte budget exceeded"
                    );
                    if done {
                        break;
                    }
                }
                delivered.sort_unstable();
                assert_eq!(delivered, (1..=count).collect::<Vec<_>>());
                assert!(checks.get() <= 12 * count + 6 * SATB_SHARDS);
                assert!(shards.drain_visits.load(Ordering::Relaxed) <= 2 * count + 1);
                println!(
                    "registry_resume owners={count} sharded={sharded} rounds={rounds} budget_checks={} delivered={count}",
                    checks.get()
                );
            }
        }
    }

    #[test]
    fn satb_registry_resume_subchunk_budgets_finish_large_buffers() {
        let coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        for sharded in [false, true] {
            let mut legacy = SatbBuffers::new(256);
            let shards = SatbShards::new(256);
            for value in 1..=65 {
                legacy.record(std::thread::current().id(), value, |_| unreachable!());
                shards
                    .with_current(|| true, |b| b.record(value, |_| unreachable!()))
                    .unwrap();
            }
            let mut values = Vec::new();
            let mut rounds = 0;
            loop {
                rounds += 1;
                assert!(rounds <= 65 + SATB_SHARDS + 3);
                let mut budget = super::super::old_cycle::RemarkBudget::new(4, 8, 10, 0);
                let before = values.len();
                let done = if sharded {
                    shards.seal_stopped_bounded_with_clock(
                        &coord,
                        &mut budget,
                        || 0,
                        |v| values.extend_from_slice(v),
                    )
                } else {
                    legacy.flush_all_bounded_with_clock(
                        &coord,
                        &mut budget,
                        || 0,
                        |v| values.extend_from_slice(v),
                    )
                };
                assert!(values.len() - before <= 1);
                if done {
                    break;
                }
            }
            assert_eq!(values, (1..=65).collect::<Vec<_>>());
        }
    }

    #[test]
    fn satb_registry_resume_handles_unregister_reregister_and_reopen() {
        let ids: Vec<_> = (0..3)
            .map(|_| {
                std::thread::spawn(|| std::thread::current().id())
                    .join()
                    .unwrap()
            })
            .collect();
        let coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        let mut legacy = SatbBuffers::new(256);
        let mut delivered = Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            legacy.record(id, i + 1, |_| unreachable!());
        }
        assert!(!legacy.flush_all_bounded_with_clock(
            &coord,
            &mut super::super::old_cycle::RemarkBudget::new(3, 8, 10, 0),
            || 0,
            |v| delivered.extend_from_slice(v)
        ));
        assert_eq!(delivered, [1]);
        legacy.flush_thread(ids[1], true, |v| delivered.extend_from_slice(v));
        legacy.record(ids[0], 4, |_| unreachable!());
        legacy.record(ids[1], 5, |_| unreachable!());
        while !legacy.flush_all_bounded_with_clock(
            &coord,
            &mut super::super::old_cycle::RemarkBudget::new(3, 8, 10, 0),
            || 0,
            |v| delivered.extend_from_slice(v),
        ) {}
        assert_eq!(delivered, [1, 2, 3, 4, 5]);
        assert!(legacy.is_empty());
        let shards = SatbShards::new(256);
        shards
            .with_current(|| true, |b| b.record(7, |_| unreachable!()))
            .unwrap();
        let mut rounds = 0;
        while !shards.seal_stopped_bounded_with_clock(
            &coord,
            &mut super::super::old_cycle::RemarkBudget::new(4, 8, 10, 0),
            || 0,
            |v| delivered.extend_from_slice(v),
        ) {
            rounds += 1;
            assert!(rounds < SATB_SHARDS + 4);
            shards.retire_current(|v| delivered.extend_from_slice(v));
            assert!(shards.with_current(|| true, |_| ()).is_none());
        }
        assert_eq!(delivered, [1, 2, 3, 4, 5, 7]);
        shards.reopen_stopped(&coord);
        shards
            .with_current(|| true, |b| b.record(8, |_| unreachable!()))
            .unwrap();
        shards.seal_stopped(&coord, |v| delivered.extend_from_slice(v));
        assert_eq!(delivered, [1, 2, 3, 4, 5, 7, 8]);
    }

    #[test]
    fn satb_many_owners_tiny_budget_and_fake_clock_have_constant_ownership_checks() {
        for count in [1, 64, 1024] {
            let mut coord = super::super::GcCoord {
                stop_requested: true,
                ..Default::default()
            };
            let mut legacy = SatbBuffers::new(256);
            let shards = SatbShards::new(256);
            for _ in 0..count {
                let id = std::thread::spawn(|| std::thread::current().id())
                    .join()
                    .unwrap();
                coord.mutators.insert(id, Default::default());
                coord.parked.insert(id);
                // Empty foreign buffers plus one retained nonempty owner.
                legacy.entries.insert(id, EntryBuffer::default());
                shards.shards[0]
                    .0
                    .lock()
                    .unwrap()
                    .insert(id, Arc::new(MutatorBuffer::default()));
            }
            legacy.record(std::thread::current().id(), 99, |_| unreachable!());
            shards
                .with_current(|| true, |b| b.record(99, |_| unreachable!()))
                .unwrap();
            for sharded in [false, true] {
                for deadline in [false, true] {
                    let checks = std::cell::Cell::new(0);
                    let mut now = || {
                        let n = checks.get();
                        checks.set(n + 1);
                        if deadline && n > 0 { 10 } else { 0 }
                    };
                    let mut budget = super::super::old_cycle::RemarkBudget::new(
                        if deadline { 1000000 } else { 1 },
                        1048576,
                        10,
                        0,
                    );
                    let mut publish =
                        |_: &[usize]| panic!("rejected transfer must preserve entries");
                    let result = if sharded {
                        shards.seal_stopped_bounded_with_clock(
                            &coord,
                            &mut budget,
                            &mut now,
                            &mut publish,
                        )
                    } else {
                        legacy.flush_all_bounded_with_clock(
                            &coord,
                            &mut budget,
                            &mut now,
                            &mut publish,
                        )
                    };
                    assert!(!result);
                    assert_eq!(
                        checks.get(),
                        2,
                        "one admitted ownership check then one rejected registry visit, independent of owner count"
                    );
                    println!(
                        "satb owners={count} sharded={sharded} deadline={deadline} ownership_checks=1 budget_checks=2 published_entries=0 retained_entries=1"
                    );
                }
            }
            let mut published = Vec::new();
            shards.seal_stopped(&coord, |v| published.extend_from_slice(v));
            assert_eq!(published, [99]);
            published.clear();
            legacy.flush_all(|v| published.extend_from_slice(v));
            assert_eq!(published, [99]);
        }
    }

    #[test]
    fn legacy_final_remark_many_owner_emptiness_is_constant_and_unwind_safe() {
        for count in [1, 64, 1024] {
            let coord = super::super::GcCoord {
                stop_requested: true,
                ..Default::default()
            };
            let mut buffers = SatbBuffers::new(256);
            for _ in 0..count {
                let id = std::thread::spawn(|| std::thread::current().id())
                    .join()
                    .unwrap();
                buffers.entries.insert(id, EntryBuffer::default());
            }
            buffers.record(std::thread::current().id(), 99, |_| unreachable!());
            assert!(!buffers.is_empty());
            buffers.flush_all(|values| assert_eq!(values, [99]));
            let visits = std::cell::Cell::new(0);
            let mut budget = super::super::old_cycle::RemarkBudget::new(1, 1, 10, 0);
            // Models final remark: the maintained invariant reads no owner
            // record; deadline admission can reject immediately afterward.
            let entry_reads = ENTRY_READS.get();
            let empty_checks = LEGACY_EMPTY_CHECKS.get();
            assert!(buffers.is_empty());
            assert_eq!(
                ENTRY_READS.get() - entry_reads,
                0,
                "final invariant must not inspect any retained buffer"
            );
            assert_eq!(LEGACY_EMPTY_CHECKS.get() - empty_checks, 1);
            assert!(!budget.reserve(0, 0, {
                visits.set(visits.get() + 1);
                10
            }));
            assert_eq!(visits.get(), 1);
            assert_eq!(
                buffers.entries.len(),
                count + 1,
                "retained empty records exercise final check"
            );
            println!(
                "final_remark legacy_owners={count} emptiness_scalar_reads=1 owner_record_visits=0 budget_checks=1 expired=true"
            );
            buffers.record(std::thread::current().id(), 7, |_| unreachable!());
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || buffers.flush_all(|_| panic!("publication failed"))
                ))
                .is_err()
            );
            assert!(!buffers.is_empty());
            let mut published = Vec::new();
            buffers.flush_all_bounded_with_clock(
                &coord,
                &mut super::super::old_cycle::RemarkBudget::new(1000000, 1048576, 10, 0),
                || 0,
                |v| published.extend_from_slice(v),
            );
            assert_eq!(published, [7]);
            assert!(buffers.is_empty());
        }
        let mut buffers = SatbBuffers::new(65536);
        for value in 1..=65 {
            buffers.record(std::thread::current().id(), value, |_| unreachable!());
        }
        let coord = super::super::GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        let clock = std::cell::Cell::new(0);
        let mut delivered = Vec::new();
        assert!(!buffers.flush_all_bounded_with_clock(
            &coord,
            &mut super::super::old_cycle::RemarkBudget::new(1000, 1048576, 10, 0),
            || clock.get(),
            |v| {
                delivered.extend_from_slice(v);
                clock.set(10);
            }
        ));
        assert_eq!(buffers.pending_entries, 33);
        assert!(!buffers.is_empty());
        buffers.flush_thread(std::thread::current().id(), true, |v| {
            delivered.extend_from_slice(v)
        });
        assert_eq!(delivered, (1..=65).collect::<Vec<_>>());
        assert!(buffers.is_empty());
        // Capacity publication also updates accounting on normal and unwind paths.
        let mut buffers = SatbBuffers::new(1);
        buffers.record(std::thread::current().id(), 1, |_| {});
        assert!(buffers.is_empty());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| buffers.record(
                std::thread::current().id(),
                2,
                |_| panic!("full publish failed")
            )))
            .is_err()
        );
        assert!(!buffers.is_empty());
        buffers.flush_thread(std::thread::current().id(), true, |v| assert_eq!(v, [2]));
        assert!(buffers.is_empty());
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
    fn retirement_publication_unwind_preserves_buffer_for_authoritative_seal() {
        let shards = SatbShards::new(256);
        shards
            .with_current(|| true, |buffer| buffer.record(123, |_| unreachable!()))
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                shards.retire_current(|_| panic!("injected publication failure"));
            }))
            .is_err()
        );
        let mut published = Vec::new();
        shards.seal(|values| published.extend_from_slice(values));
        assert_eq!(published, vec![123]);
        shards.retire_current(|_| unreachable!());
        assert!(shards.is_empty());
    }

    #[test]
    fn concurrent_seal_and_unregister_transfer_each_entry_exactly_once() {
        for count in [1, 16, 80] {
            let buffers = Arc::new(SatbShards::new(256));
            let published = Arc::new(Mutex::new(Vec::new()));
            let ready = Arc::new(std::sync::Barrier::new(count + 1));
            let retire = Arc::new(std::sync::Barrier::new(count + 1));
            std::thread::scope(|scope| {
                for value in 1..=count {
                    let (buffers, published, ready, retire) = (
                        buffers.clone(),
                        published.clone(),
                        ready.clone(),
                        retire.clone(),
                    );
                    scope.spawn(move || {
                        buffers
                            .with_current(
                                || true,
                                |buffer| buffer.record(value, |_| unreachable!()),
                            )
                            .unwrap();
                        ready.wait();
                        retire.wait();
                        buffers.retire_current(|values| {
                            published.lock().unwrap().extend_from_slice(values)
                        });
                    });
                }
                ready.wait();
                buffers.seal_without_drain();
                retire.wait();
                buffers.seal(|values| published.lock().unwrap().extend_from_slice(values));
            });
            let mut values = published.lock().unwrap().clone();
            values.sort_unstable();
            assert_eq!(values, (1..=count).collect::<Vec<_>>());
            assert!(buffers.is_drained());
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
                                        &buffer.entries[..],
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
                        buffers.retire_current(|_| panic!("owner flush must precede retirement"));
                    });
                }
                ready.wait();
                let registered_count: usize = buffers
                    .shards
                    .iter()
                    .map(|shard| shard.0.lock().unwrap().entries.len())
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
                    .all(|shard| shard.0.lock().unwrap().entries.is_empty())
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

#[cfg(test)]
mod m3_measurements {
    use super::*;
    #[test]
    fn m3_satb_contention_and_seal_capture_all_publications() {
        let _guard = super::super::runtime_test_guard();
        for threads in [1, 8, 64, 128] {
            let shards = Arc::new(SatbShards::new(256));
            let start = Arc::new(std::sync::Barrier::new(threads));
            let count = Arc::new(AtomicUsize::new(0));
            let before = crate::gc_telemetry::m3::snapshot();
            let began = std::time::Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    let (shards, start, count) = (shards.clone(), start.clone(), count.clone());
                    scope.spawn(move || {
                        start.wait();
                        for i in 0..4096 {
                            shards
                                .with_current(
                                    || true,
                                    |buffer| {
                                        buffer.record(i + 1, |entries| {
                                            count.fetch_add(entries.len(), Ordering::Relaxed);
                                        })
                                    },
                                )
                                .unwrap();
                        }
                    });
                }
            });
            let coord = super::super::GcCoord {
                stop_requested: true,
                ..Default::default()
            };
            let seal = std::time::Instant::now();
            shards.seal_stopped(&coord, |entries| {
                count.fetch_add(entries.len(), Ordering::Relaxed);
            });
            let seal_ns = seal.elapsed().as_nanos();
            assert!(shards.is_drained());
            assert_eq!(count.load(Ordering::Relaxed), threads * 4096);
            assert!(shards.with_current(|| true, |_| ()).is_none());
            let after = crate::gc_telemetry::m3::snapshot();
            println!(
                "m3_satb threads={threads} stores={} elapsed_ns={} seal_ns={seal_ns} registry_wait_ns={} registry_collisions={} buffer_wait_ns={}",
                threads * 4096,
                began.elapsed().as_nanos(),
                after.intervals[0].total_ns - before.intervals[0].total_ns,
                after.collisions[0] - before.collisions[0],
                after.intervals[1].total_ns - before.intervals[1].total_ns
            );
        }
    }
}
