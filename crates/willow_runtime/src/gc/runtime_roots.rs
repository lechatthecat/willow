use crate::id_hash::{IdMap, hash_u64};
use std::sync::{Mutex, MutexGuard};

const SHARD_COUNT: usize = 32;
#[derive(Default)]
struct Roots {
    // The map is only an index; snapshots never enumerate retained buckets.
    indices: IdMap<usize, usize>,
    entries: Vec<(usize, usize)>, // (address, owner count)
    pending_indices: IdMap<usize, usize>,
    pending: Vec<usize>,
}

impl Roots {
    fn remove_pending(&mut self, address: usize) {
        if let Some(index) = self.pending_indices.remove(&address) {
            self.pending.swap_remove(index);
            if let Some(&moved) = self.pending.get(index) {
                *self.pending_indices.get_mut(&moved).unwrap() = index;
            }
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn addresses(&self) -> impl Iterator<Item = usize> + '_ {
        self.entries.iter().map(|&(address, _)| address)
    }
}

/// Reference-counted roots owned by runtime structures. Each object belongs to
/// one shard, so unrelated lifecycle operations need not share a registry lock.
/// Callers retain responsibility for GC publication/deletion barriers.
#[derive(Default)]
pub(super) struct RuntimeRootSet {
    shards: [Mutex<Roots>; SHARD_COUNT],
}

impl RuntimeRootSet {
    fn shard_index(root: usize) -> usize {
        // Hash the full address: low bits alone collapse aligned objects and
        // page/region-spaced allocations onto the same lock.
        hash_u64(root as u64) as usize % SHARD_COUNT
    }

    pub(super) fn add(&self, object: *mut u8) {
        self.add_inner(object, false);
    }

    pub(super) fn add_pending(&self, object: *mut u8) {
        self.add_inner(object, true);
    }

    fn add_inner(&self, object: *mut u8, pending: bool) {
        if object.is_null() {
            return;
        }
        let root = object as usize;
        let mut roots = self.shards[Self::shard_index(root)].lock().unwrap();
        if let Some(&index) = roots.indices.get(&root) {
            roots.entries[index].1 += 1;
        } else {
            let index = roots.entries.len();
            roots.entries.push((root, 1));
            roots.indices.insert(root, index);
        }
        if pending && !roots.pending_indices.contains_key(&root) {
            let index = roots.pending.len();
            roots.pending_indices.insert(root, index);
            roots.pending.push(root);
        }
    }

    pub(super) fn remove(&self, object: *mut u8) {
        if object.is_null() {
            return;
        }
        let root = object as usize;
        let mut roots = self.shards[Self::shard_index(root)].lock().unwrap();
        if let Some(&index) = roots.indices.get(&root) {
            if roots.entries[index].1 > 1 {
                roots.entries[index].1 -= 1;
            } else {
                roots.indices.remove(&root);
                roots.entries.swap_remove(index);
                if let Some(&(moved, _)) = roots.entries.get(index) {
                    *roots.indices.get_mut(&moved).unwrap() = index;
                }
            }
        }
    }

    /// Reclamation, unlike root removal, ends an owner's scan obligation.
    /// Call under the heap lock before managed storage becomes reusable.
    pub(super) fn forget_pending(&self, address: usize) {
        self.shards[Self::shard_index(address)]
            .lock()
            .unwrap()
            .remove_pending(address);
    }

    /// Visit initial-child scan obligations, including owners no longer rooted
    /// here. The caller holds the heap lock and validates addresses before use;
    /// neither a pending address nor its shard lock keeps its object alive.
    pub(super) fn drain_pending(&self, mut visit: impl FnMut(usize)) {
        for shard in &self.shards {
            let mut roots = shard.lock().unwrap();
            while let Some(root) = roots.pending.pop() {
                roots.pending_indices.remove(&root);
                visit(root);
            }
        }
    }

    fn lock_all(&self) -> [MutexGuard<'_, Roots>; SHARD_COUNT] {
        // Preserve the original atomic snapshot/count/reset semantics. Every
        // multi-shard operation locks in ascending order; add/remove hold only
        // one lock and never call back into the collector while holding it.
        std::array::from_fn(|index| self.shards[index].lock().unwrap())
    }

    pub(super) fn snapshot(&self) -> Vec<*mut u8> {
        let shards = self.lock_all();
        let mut roots = Vec::with_capacity(shards.iter().map(|shard| shard.len()).sum());
        for shard in &shards {
            roots.extend(shard.addresses().map(|root| root as *mut u8));
        }
        roots
    }

    /// Snapshot referents while registration still protects native slot storage.
    /// Returning slot addresses and then dereferencing them after unlocking is
    /// invalid for short-lived native handles during concurrent marking.
    ///
    /// # Safety
    /// Every registered address must name a live, aligned pointer slot. Native
    /// writers use atomic stores; generated/foreign slots have no concurrent
    /// non-atomic writer while registered. Removal precedes freeing storage.
    pub(super) unsafe fn snapshot_slot_values(&self) -> Vec<*mut u8> {
        let shards = self.lock_all();
        let mut values = Vec::with_capacity(shards.iter().map(|shard| shard.len()).sum());
        for shard in &shards {
            for address in shard.addresses() {
                let value = unsafe { crate::gc::load_gc_reference(address as *mut *mut u8) };
                if !value.is_null() {
                    values.push(value);
                }
            }
        }
        values
    }

    pub(super) fn len(&self) -> usize {
        self.lock_all().iter().map(|shard| shard.len()).sum()
    }

    #[cfg(test)]
    pub(super) fn pending_count(&self) -> usize {
        self.lock_all()
            .iter()
            .map(|shard| shard.pending.len())
            .sum()
    }

    pub(super) fn clear(&self) {
        for shard in &mut self.lock_all() {
            **shard = Roots::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn pending_owners_survive_root_removal_until_scan_or_reclamation() {
        for count in [64, 256, 1024, 4096] {
            let roots = RuntimeRootSet::default();
            for i in 1..=count {
                roots.add_pending((i * 16) as *mut u8);
                roots.add_pending((i * 16) as *mut u8);
                roots.remove((i * 16) as *mut u8);
                if i % 16 != 0 {
                    roots.remove((i * 16) as *mut u8);
                }
            }
            assert_eq!(roots.len(), count / 16);
            assert_eq!(roots.snapshot().len(), count / 16);
            // Repeated registrations after losing all roots still deduplicate.
            roots.add_pending(16 as *mut u8);
            roots.remove(16 as *mut u8);
            for i in (1..=count).filter(|i| i % 16 != 0 && i % 2 == 0) {
                roots.forget_pending(i * 16);
            }
            let mut visits = 0;
            roots.drain_pending(|address| {
                visits += 1;
                assert!(address / 16 % 2 == 1 || address % 256 == 0);
            });
            assert_eq!(visits, count / 2 + count / 16);
            roots.drain_pending(|_| panic!("unchanged roots must not be scanned again"));
            // Re-registration of an existing owner may introduce new children.
            roots.add_pending(256 as *mut u8);
            roots.drain_pending(|address| assert_eq!(address, 256));
            roots.clear();
            roots.drain_pending(|_| panic!("reset must clear pending work"));
            eprintln!(
                "registrations={} pending_visits={visits} repeated_visits=0",
                count * 2
            );
        }
    }

    #[test]
    fn lifecycle_touches_only_the_address_shard() {
        let roots = RuntimeRootSet::default();
        let mut object = 0u8;
        let pointer = &mut object as *mut u8;
        let target = RuntimeRootSet::shard_index(pointer as usize);
        // Holding every other shard proves add/remove need exactly one lock.
        let _other_guards: Vec<_> = roots
            .shards
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != target)
            .map(|(_, shard)| shard.lock().unwrap())
            .collect();
        roots.add(pointer);
        roots.add(pointer);
        roots.remove(pointer);
        assert_eq!(
            roots.shards[target].lock().unwrap().entries,
            vec![(pointer as usize, 1)]
        );
        roots.remove(pointer);
        roots.remove(pointer);
        assert!(roots.shards[target].lock().unwrap().entries.is_empty());
        roots.add(std::ptr::null_mut());
        roots.remove(std::ptr::null_mut());
    }

    #[test]
    fn aggregate_guard_holds_every_shard() {
        let roots = RuntimeRootSet::default();
        let guards = roots.lock_all();
        for shard in &roots.shards {
            assert!(matches!(
                shard.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
        }
        drop(guards);
        for shard in &roots.shards {
            assert!(shard.try_lock().is_ok());
        }
    }

    #[test]
    fn aligned_and_fragmented_roots_scale_by_distinct_addresses() {
        for stride in [16, 4096, 256 * 1024] {
            for distinct in [64, 256, 1024, 4096] {
                let roots = RuntimeRootSet::default();
                // Opaque addresses are never dereferenced by the registry.
                for index in 1..=distinct {
                    for _ in 0..4 {
                        roots.add((index * stride) as *mut u8);
                    }
                }
                let guards = roots.lock_all();
                let occupied = guards
                    .iter()
                    .filter(|shard| !shard.entries.is_empty())
                    .count();
                assert!(
                    occupied > SHARD_COUNT / 2,
                    "aligned roots collapsed onto too few locks"
                );
                assert_eq!(
                    guards
                        .iter()
                        .flat_map(|shard| shard.entries.iter().map(|&(_, count)| count))
                        .sum::<usize>(),
                    distinct * 4
                );
                drop(guards);
                let mut actual: Vec<_> = roots.snapshot().into_iter().map(|p| p as usize).collect();
                actual.sort_unstable();
                assert_eq!(
                    actual,
                    (1..=distinct).map(|i| i * stride).collect::<Vec<_>>()
                );
                // Fragment the tables, then remove duplicate owners of survivors.
                for index in 1..=distinct {
                    let releases = if index % 16 == 0 { 3 } else { 4 };
                    for _ in 0..releases {
                        roots.remove((index * stride) as *mut u8);
                    }
                }
                assert_eq!(roots.len(), distinct / 16);
                assert_eq!(roots.snapshot().len(), distinct / 16);
                roots.clear();
                assert_eq!(roots.len(), 0);
                assert!(roots.snapshot().is_empty());
                eprintln!(
                    "stride={stride} distinct={distinct} add_locks={} remove_locks={} snapshot_locks={SHARD_COUNT} snapshot_entries={distinct} fragmented_entries={} occupied_shards={occupied}",
                    distinct * 4,
                    distinct * 4 - distinct / 16,
                    distinct / 16
                );
            }
        }
    }

    #[test]
    fn snapshot_scans_only_live_entries_after_repeated_churn() {
        for peak in [64, 256, 1024, 4096] {
            let roots = RuntimeRootSet::default();
            for cycle in 0..3 {
                for index in 1..=peak {
                    roots.add((index * 4096) as *mut u8);
                    roots.add((index * 4096) as *mut u8);
                }
                // Alternate removal order to exercise both swap and tail removal.
                for offset in 1..=peak {
                    let index = if cycle % 2 == 0 {
                        offset
                    } else {
                        peak + 1 - offset
                    };
                    roots.remove((index * 4096) as *mut u8);
                    if index % 16 != 0 {
                        roots.remove((index * 4096) as *mut u8);
                    }
                }
                let guards = roots.lock_all();
                let mut scanned = 0;
                let capacity: usize = guards.iter().map(|s| s.indices.capacity()).sum();
                for shard in &guards {
                    assert_eq!(shard.indices.len(), shard.entries.len());
                    // Count the exact dense iterator used by snapshot, including
                    // validating every moved entry's reverse index and owner count.
                    for address in shard.addresses().inspect(|_| scanned += 1) {
                        let index = shard.indices[&address];
                        assert_eq!(shard.entries[index], (address, 1));
                        assert_eq!(address % (16 * 4096), 0);
                    }
                }
                assert_eq!(scanned, peak / 16);
                assert!(capacity > scanned);
                drop(guards);
                let mut snapshot: Vec<_> =
                    roots.snapshot().into_iter().map(|p| p as usize).collect();
                snapshot.sort_unstable();
                assert_eq!(
                    snapshot,
                    (1..=peak / 16).map(|i| i * 16 * 4096).collect::<Vec<_>>()
                );
                eprintln!(
                    "peak={peak} cycle={cycle} retained_buckets={capacity} scanned={scanned}"
                );
                for index in (16..=peak).step_by(16) {
                    roots.remove((index * 4096) as *mut u8);
                }
                assert!(roots.snapshot().is_empty());
            }
            roots.clear();
            assert!(
                roots
                    .lock_all()
                    .iter()
                    .all(|s| s.entries.capacity() == 0 && s.indices.capacity() == 0)
            );
        }
    }

    #[test]
    fn concurrent_duplicate_owners_and_snapshots() {
        const WORKERS: usize = 8;
        const DISTINCT: usize = 256;
        let roots = RuntimeRootSet::default();
        let barrier = Barrier::new(WORKERS + 1);
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                scope.spawn(|| {
                    for index in 1..=DISTINCT {
                        roots.add((index * 16) as *mut u8);
                    }
                    barrier.wait();
                    barrier.wait();
                    for index in 1..=DISTINCT {
                        roots.remove((index * 16) as *mut u8);
                    }
                });
            }
            barrier.wait();
            assert_eq!(roots.len(), DISTINCT);
            assert_eq!(roots.snapshot().len(), DISTINCT);
            assert!(
                roots
                    .lock_all()
                    .iter()
                    .flat_map(|shard| shard.entries.iter().map(|&(_, count)| count))
                    .all(|count| count == WORKERS)
            );
            barrier.wait();
            for _ in 0..32 {
                let snapshot = roots.snapshot();
                let unique: std::collections::HashSet<_> = snapshot.iter().copied().collect();
                assert_eq!(unique.len(), snapshot.len());
                assert!(
                    snapshot
                        .iter()
                        .all(|&p| (1..=DISTINCT).contains(&(p as usize / 16)))
                );
            }
        });
        assert_eq!(roots.len(), 0);
    }
}
