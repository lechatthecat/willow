//! Task records under one shard mutex. Keep the first record directly in the
//! shard: channel ownership repeatedly looks up the same small set of tasks,
//! and a hash lookup buys nothing when a shard contains just one of them.
use super::{RuntimeTask, RuntimeTaskId};
use crate::id_hash::IdMap;

#[derive(Debug, Default)]
pub(super) struct TaskShard {
    // Even while vacant, first_id never occurs in rest. Keeping it outside
    // the Option lets overflow lookups miss with just one comparison.
    first_id: RuntimeTaskId,
    first: Option<RuntimeTask>,
    // IDs in a shard share their low bits. The folded-multiply IdMap hasher
    // spreads the remaining bits across both bucket positions and control tags.
    rest: IdMap<RuntimeTaskId, RuntimeTask>,
    #[cfg(test)]
    lookups: std::cell::Cell<usize>,
}

impl TaskShard {
    pub(super) fn get(&self, id: &RuntimeTaskId) -> Option<&RuntimeTask> {
        if self.first_id == *id {
            return self.first.as_ref();
        }
        #[cfg(test)]
        self.lookups.set(self.lookups.get() + 1);
        self.rest.get(id)
    }

    pub(super) fn get_mut(&mut self, id: &RuntimeTaskId) -> Option<&mut RuntimeTask> {
        if self.first_id == *id {
            return self.first.as_mut();
        }
        #[cfg(test)]
        self.lookups.set(self.lookups.get() + 1);
        self.rest.get_mut(id)
    }

    pub(super) fn get_disjoint_mut(
        &mut self,
        ids: [&RuntimeTaskId; 2],
    ) -> [Option<&mut RuntimeTask>; 2] {
        assert_ne!(ids[0], ids[1], "duplicate task ids");
        if let Some(task) = &mut self.first {
            if self.first_id == *ids[0] {
                return [Some(task), self.rest.get_mut(ids[1])];
            }
            if self.first_id == *ids[1] {
                return [self.rest.get_mut(ids[0]), Some(task)];
            }
        }
        self.rest.get_disjoint_mut(ids)
    }

    pub(super) fn insert(&mut self, id: RuntimeTaskId, task: RuntimeTask) {
        if let Some(first) = &mut self.first {
            if self.first_id == id {
                *first = task;
                return;
            }
        } else {
            // A removed first record leaves the overflow untouched. Replacing
            // an overflow id must not create a second record in the first slot.
            self.rest.remove(&id);
            self.first_id = id;
            self.first = Some(task);
            return;
        }
        self.rest.insert(id, task);
    }

    pub(super) fn remove(&mut self, id: &RuntimeTaskId) -> Option<RuntimeTask> {
        if self.first_id == *id {
            // Do not scan sparse overflow buckets to refill the direct slot.
            // A subsequent insert reuses it in constant work.
            return self.first.take();
        }
        self.rest.remove(id)
    }

    pub(super) fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &RuntimeTask> {
        self.first.iter().chain(self.rest.values())
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = (RuntimeTaskId, RuntimeTask)> {
        self.first
            .take()
            .map(|task| (self.first_id, task))
            .into_iter()
            .chain(self.rest.drain())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_churn_needs_no_table_allocation_or_lookup() {
        for count in [1, 32, 1024, 32768] {
            let mut shard = TaskShard::default();
            for id in 1..=count {
                shard.insert(id, RuntimeTask::new(id));
                assert_eq!(shard.get(&id).unwrap().id, id);
                shard.get_mut(&id).unwrap().started = true;
                assert!(shard.remove(&id).unwrap().started);
                assert_eq!(shard.len(), 0);
            }
            assert_eq!(shard.lookups.get(), 0);
            assert_eq!(shard.rest.capacity(), 0);
            println!("operations={count} hash_lookups=0 table_capacity=0");
        }
    }

    #[test]
    fn replacements_and_vacant_first_slot_never_duplicate_or_retain_records() {
        let mut shard = TaskShard::default();
        for id in [1, 2, 3] {
            shard.insert(id, RuntimeTask::new(id));
        }
        for id in [1, 2] {
            let old = std::sync::Arc::downgrade(&shard.get(&id).unwrap().panic_context());
            let mut replacement = RuntimeTask::new(id);
            replacement.started = true;
            shard.insert(id, replacement);
            assert!(old.upgrade().is_none());
            assert!(shard.get(&id).unwrap().started);
            assert_eq!(shard.len(), 3);
        }
        assert!(shard.remove(&1).is_some());
        let old = std::sync::Arc::downgrade(&shard.get(&2).unwrap().panic_context());
        shard.insert(2, RuntimeTask::new(2));
        assert!(old.upgrade().is_none());
        assert_eq!(shard.len(), 2);
        assert!(!shard.rest.contains_key(&2));
        assert!(shard.remove(&2).is_some());
        assert!(shard.remove(&2).is_none());
        shard.insert(4, RuntimeTask::new(4));
        let mut ids: Vec<_> = shard.values().map(|task| task.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [3, 4]);
        let mut drained: Vec<_> = shard.drain().map(|(id, task)| (id, task.id)).collect();
        drained.sort_unstable();
        assert_eq!(drained, [(3, 3), (4, 4)]);
        assert_eq!(shard.len(), 0);
        assert_eq!(shard.values().count(), 0);
        shard.insert(5, RuntimeTask::new(5));
        assert_eq!(shard.get(&5).unwrap().id, 5);
    }

    #[test]
    fn disjoint_access_covers_both_tiers_and_missing_ids() {
        let mut shard = TaskShard::default();
        for id in 1..=3 {
            shard.insert(id, RuntimeTask::new(id));
        }
        for first in 0..=4 {
            for second in 0..=4 {
                if first == second {
                    continue;
                }
                let tasks = shard.get_disjoint_mut([&first, &second]);
                for (id, task) in [first, second].into_iter().zip(tasks) {
                    assert_eq!(
                        task.as_ref().map(|task| task.id),
                        (1..=3).contains(&id).then_some(id)
                    );
                    if let Some(task) = task {
                        task.started = true;
                    }
                }
            }
        }
        assert!(shard.values().all(|task| task.started));
        // A partial drain still drops every unconsumed record, just like HashMap.
        let owners: Vec<_> = shard
            .values()
            .map(|task| std::sync::Arc::downgrade(&task.panic_context()))
            .collect();
        drop(shard.drain().next());
        assert_eq!(shard.len(), 0);
        assert!(owners.iter().all(|owner| owner.upgrade().is_none()));
    }

    #[test]
    fn fragmented_overflow_and_repeated_direct_slot_reuse_have_bounded_lookup_work() {
        for size in [8, 64, 1024, 8192] {
            let mut shard = TaskShard::default();
            for id in 1..=size {
                shard.insert(id, RuntimeTask::new(id));
            }
            for id in 1..size {
                assert!(shard.remove(&id).is_some());
            }
            shard.lookups.set(0);
            for id in (size + 1)..=(size * 2) {
                shard.insert(id, RuntimeTask::new(id));
                assert_eq!(shard.get_mut(&id).unwrap().id, id);
                assert_eq!(shard.get_mut(&size).unwrap().id, size);
                assert!(shard.remove(&id).is_some());
            }
            assert_eq!(shard.len(), 1);
            assert_eq!(shard.lookups.get(), size as usize);
            println!(
                "fragmented_peak={size} pairs={size} hash_lookups={}",
                shard.lookups.get()
            );
        }
    }

    #[test]
    fn mixed_operations_match_a_reference_map() {
        let mut shard = TaskShard::default();
        let mut reference = std::collections::BTreeMap::new();
        let mut seed = 12345u64;
        for step in 0..20000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = (seed >> 32) % 97;
            match seed % 4 {
                0 => {
                    let mut task = RuntimeTask::new(id);
                    task.started = step % 3 == 0;
                    reference.insert(id, task.started);
                    shard.insert(id, task);
                }
                1 => assert_eq!(
                    shard.remove(&id).map(|task| task.started),
                    reference.remove(&id)
                ),
                2 => {
                    if let Some(task) = shard.get_mut(&id) {
                        task.started = !task.started;
                    }
                    if let Some(started) = reference.get_mut(&id) {
                        *started = !*started;
                    }
                }
                _ => assert_eq!(
                    shard.get(&id).map(|task| task.started),
                    reference.get(&id).copied()
                ),
            }
            assert_eq!(shard.len(), reference.len());
        }
        let actual: std::collections::BTreeMap<_, _> =
            shard.drain().map(|(id, task)| (id, task.started)).collect();
        assert_eq!(actual, reference);
    }
}
