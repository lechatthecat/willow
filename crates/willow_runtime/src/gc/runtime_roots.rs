use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

const SHARD_COUNT: usize = 32;
type Roots = HashMap<usize, usize>;

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
        let mut hasher = DefaultHasher::new();
        root.hash(&mut hasher);
        hasher.finish() as usize % SHARD_COUNT
    }

    pub(super) fn add(&self, object: *mut u8) {
        if object.is_null() {
            return;
        }
        let root = object as usize;
        let mut roots = self.shards[Self::shard_index(root)].lock().unwrap();
        *roots.entry(root).or_insert(0) += 1;
    }

    pub(super) fn remove(&self, object: *mut u8) {
        if object.is_null() {
            return;
        }
        let root = object as usize;
        let mut roots = self.shards[Self::shard_index(root)].lock().unwrap();
        if let Some(count) = roots.get_mut(&root) {
            if *count > 1 {
                *count -= 1;
            } else {
                roots.remove(&root);
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
            roots.extend(shard.keys().map(|&root| root as *mut u8));
        }
        roots
    }

    pub(super) fn len(&self) -> usize {
        self.lock_all().iter().map(|shard| shard.len()).sum()
    }

    pub(super) fn clear(&self) {
        for shard in &mut self.lock_all() {
            shard.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

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
            roots.shards[target]
                .lock()
                .unwrap()
                .get(&(pointer as usize)),
            Some(&1)
        );
        roots.remove(pointer);
        roots.remove(pointer);
        assert!(roots.shards[target].lock().unwrap().is_empty());
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
                let occupied = guards.iter().filter(|shard| !shard.is_empty()).count();
                assert!(
                    occupied > SHARD_COUNT / 2,
                    "aligned roots collapsed onto too few locks"
                );
                assert_eq!(
                    guards
                        .iter()
                        .flat_map(|shard| shard.values())
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
                    .flat_map(|shard| shard.values())
                    .all(|&count| count == WORKERS)
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
