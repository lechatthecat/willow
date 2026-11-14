//! Statically dispatched epoch observations. Unit observers have no hot-path work.
pub(super) trait RegionEvents {
    fn lookup(&self) {}
    fn reset_pages(&mut self) {}
    fn page(&mut self) {}
}
impl RegionEvents for NoObservation {}
pub(super) trait EpochEvents {
    fn read(&self) {}
    fn claim(&self) {}
    fn acquired(&self) {}
}
impl EpochEvents for NoObservation {}
impl EpochEvents for std::marker::PhantomData<&()> {}
pub(super) trait ViewEvents: Default {
    fn share(&self) -> Self;
    fn add(&self) {}
}
impl ViewEvents for NoObservation {
    fn share(&self) -> Self {
        *self
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct NoObservation {
    _private: (),
}
impl NoObservation {
    pub(super) fn borrow(&self) -> std::marker::PhantomData<&()> {
        std::marker::PhantomData
    }
}
#[cfg(not(test))]
pub(super) type EpochObservationRef<'a> = std::marker::PhantomData<&'a ()>;
#[cfg(test)]
pub(super) type EpochObservationRef<'a> = &'a EpochObservation;
#[cfg(not(test))]
const _: () = assert!(std::mem::size_of::<EpochObservationRef<'static>>() == 0);

#[cfg(not(test))]
pub(super) type RegionObservation = NoObservation;
#[cfg(not(test))]
pub(super) type EpochObservation = NoObservation;
#[cfg(not(test))]
pub(super) type ViewObservation = NoObservation;
#[cfg(test)]
pub(super) use tests::{EpochObservation, RegionObservation, ViewObservation};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{ThreadId, current};

    #[derive(Default)]
    pub(crate) struct RegionObservation {
        lookups: Mutex<HashMap<ThreadId, usize>>,
        pub(crate) page_visits: usize,
    }
    impl RegionEvents for RegionObservation {
        fn lookup(&self) {
            *self
                .lookups
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default() += 1;
        }
        fn reset_pages(&mut self) {
            self.page_visits = 0;
        }
        fn page(&mut self) {
            self.page_visits += 1;
        }
    }
    impl RegionObservation {
        pub(crate) fn reset_lookups(&self) {
            self.lookups.lock().unwrap().remove(&current().id());
        }
        pub(crate) fn lookups(&self) -> usize {
            self.lookups
                .lock()
                .unwrap()
                .get(&current().id())
                .copied()
                .unwrap_or(0)
        }
    }

    #[derive(Default)]
    struct ThreadCounts {
        reads: usize,
        claims: usize,
        hook: Option<Box<dyn FnOnce() + Send>>,
    }
    #[derive(Default)]
    pub(crate) struct EpochObservation(Mutex<HashMap<ThreadId, ThreadCounts>>);
    impl EpochEvents for EpochObservation {
        fn read(&self) {
            self.0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .reads += 1;
        }
        fn claim(&self) {
            self.0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .claims += 1;
        }
        fn acquired(&self) {
            // The callback may reenter epoch services. Release observation
            // storage before invoking it and take it exactly once.
            let hook = self
                .0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .hook
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
    impl EpochObservation {
        pub(crate) fn borrow(&self) -> &Self {
            self
        }
        pub(crate) fn reads(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .map_or(0, |v| v.reads)
        }
        pub(crate) fn claims(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .map_or(0, |v| v.claims)
        }
        pub(crate) fn reset_reads(&self) {
            self.0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .reads = 0;
        }
        pub(crate) fn set_hook(&self, hook: impl FnOnce() + Send + 'static) {
            self.0
                .lock()
                .unwrap()
                .entry(current().id())
                .or_default()
                .hook = Some(Box::new(hook));
        }
        pub(crate) fn has_hook(&self) -> bool {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .is_some_and(|v| v.hook.is_some())
        }
    }

    #[derive(Default)]
    pub(crate) struct ViewObservation(Arc<AtomicU64>);
    impl ViewEvents for ViewObservation {
        fn share(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
        fn add(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl ViewObservation {
        pub(crate) fn count(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }
    #[test]
    fn epoch_observation_owners_and_threads_are_independent() {
        let first = EpochObservation::default();
        let second = EpochObservation::default();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [1, 3]
                .into_iter()
                .map(|stride| {
                    let (first, second, barrier) = (&first, &second, &barrier);
                    scope.spawn(crate::runtime::serving(move || {
                        let mut correct = true;
                        for round in 1..=64 {
                            for _ in 0..stride {
                                first.read();
                                second.claim();
                            }
                            barrier.wait();
                            correct &= first.reads() == round * stride && first.claims() == 0;
                            correct &= second.claims() == round * stride && second.reads() == 0;
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
        assert_eq!(first.reads(), 0);
        assert_eq!(second.claims(), 0);
        let first = Arc::new(first);
        let weak = Arc::downgrade(&first);
        first.set_hook(move || {
            let owner = weak.upgrade().unwrap();
            assert!(!owner.has_hook(), "hook is taken before reentry");
            owner.read();
        });
        first.acquired();
        first.acquired();
        assert_eq!(first.reads(), 1, "hook executes exactly once");
        let views = ViewObservation::default();
        let shared = views.share();
        views.add();
        shared.add();
        assert_eq!(views.count(), 2);
        assert_eq!(ViewObservation::default().count(), 0);
    }
}
