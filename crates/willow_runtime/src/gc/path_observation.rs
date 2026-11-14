//! Instance-owned observations of allocation and root-publication paths.
//! The unit observer is erased from production calls by monomorphization.

pub(super) trait PathObservation {
    // Defer the owner lookup too: a disabled observation must not force lazy
    // collector initialization or add another lookup on a buffered allocation.
    fn root_owner_slow<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn old_lab_lookup<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn old_allocation<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn card_cache_hit<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn root_slot_registration<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
}

impl PathObservation for () {}

#[cfg(not(test))]
pub(super) type RuntimePathObservation = ();
#[cfg(test)]
pub(super) type RuntimePathObservation = tests::ThreadPathCounts;

#[cfg(test)]
mod tests {
    use super::PathObservation;
    use std::collections::HashMap;
    use std::sync::{Barrier, Mutex};
    use std::thread::{ThreadId, current};

    /// Thread attribution belongs to this observer, rather than a TLS static.
    /// The map is test evidence only; production uses the unit observer.
    #[derive(Default)]
    pub(crate) struct ThreadPathCounts(Mutex<HashMap<ThreadId, Counts>>);

    #[derive(Default)]
    struct Counts {
        allocations: usize,
        roots: usize,
        card_hits: u64,
        root_owner: usize,
        old_lab_lookups: usize,
    }

    impl ThreadPathCounts {
        pub(crate) fn root_owner_slow_paths(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .map_or(0, |c| c.root_owner)
        }
        pub(crate) fn set_root_owner_slow_paths(&self, value: usize) {
            self.0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .root_owner = value;
        }
        pub(crate) fn old_lab_lookups(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .map_or(0, |c| c.old_lab_lookups)
        }
        pub(crate) fn current(&self) -> (usize, usize) {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .map(|counts| (counts.allocations, counts.roots))
                .unwrap_or_default()
        }

        pub(crate) fn card_cache_hits(&self) -> u64 {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .map_or(0, |counts| counts.card_hits)
        }

        fn record(&self, old_allocation: bool) {
            let mut threads = self.0.lock().unwrap();
            let counts = threads.entry(current().id()).or_default();
            if old_allocation {
                counts.allocations += 1;
            } else {
                counts.roots += 1;
            }
        }
    }

    impl PathObservation for ThreadPathCounts {
        fn root_owner_slow<'a>(owner: impl FnOnce() -> &'a Self) {
            owner()
                .0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .root_owner += 1;
        }
        fn old_lab_lookup<'a>(owner: impl FnOnce() -> &'a Self) {
            owner()
                .0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .old_lab_lookups += 1;
        }
        fn card_cache_hit<'a>(owner: impl FnOnce() -> &'a Self) {
            let mut threads = owner().0.lock().unwrap();
            threads.entry(current().id()).or_default().card_hits += 1;
        }

        fn old_allocation<'a>(owner: impl FnOnce() -> &'a Self) {
            owner().record(true);
        }

        fn root_slot_registration<'a>(owner: impl FnOnce() -> &'a Self) {
            owner().record(false);
        }
    }

    #[test]
    fn gc_path_observers_keep_instance_and_thread_counts_independent() {
        let first = ThreadPathCounts::default();
        let second = ThreadPathCounts::default();
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [1, 3]
                .into_iter()
                .map(|stride| {
                    let (first, second, barrier) = (&first, &second, &barrier);
                    scope.spawn(crate::runtime::serving(move || {
                        let mut correct = true;
                        for round in 1..=64 {
                            for _ in 0..stride {
                                ThreadPathCounts::old_allocation(|| first);
                                ThreadPathCounts::card_cache_hit(|| first);
                                ThreadPathCounts::root_slot_registration(|| second);
                                ThreadPathCounts::root_slot_registration(|| second);
                            }
                            barrier.wait();
                            correct &= first.current() == (round * stride, 0);
                            correct &= first.card_cache_hits() == (round * stride) as u64;
                            correct &= second.card_cache_hits() == 0;
                            correct &= second.current() == (0, round * stride * 2);
                            barrier.wait();
                        }
                        correct
                    }))
                })
                .collect();
            for handle in handles {
                assert!(handle.join().unwrap());
            }
        });
        assert_eq!(first.current(), (0, 0));
        assert_eq!(second.current(), (0, 0));
        <()>::card_cache_hit(|| panic!("disabled observation must not resolve an owner"));
        <()>::old_allocation(|| panic!("disabled observation must not resolve an owner"));
        <()>::root_slot_registration(|| panic!("disabled observation must not resolve an owner"));
    }
}
