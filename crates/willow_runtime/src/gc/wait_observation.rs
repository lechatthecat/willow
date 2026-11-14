//! Collector-owned wait observations; disabled observation does not evaluate
//! its context closure or inspect the heap.

pub(super) trait WaitObservation {
    fn reader_wait<'a>(_owner: impl FnOnce() -> (&'a Self, bool))
    where
        Self: 'a,
    {
    }
    fn recovery_sample<'a>(_owner: impl FnOnce() -> (&'a Self, [u64; 4]))
    where
        Self: 'a,
    {
    }

    fn coordinator_wait<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn before_coordinator_collection<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn checked<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn initial_wait<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn sleeping<'a>(_owner: impl FnOnce() -> (&'a Self, bool))
    where
        Self: 'a,
    {
    }
    fn before_initial<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
}
impl WaitObservation for () {}
#[cfg(not(test))]
pub(super) type RuntimeWaitObservation = ();
#[cfg(test)]
pub(super) type RuntimeWaitObservation = tests::Observation;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::thread::{ThreadId, current};

    #[derive(Default)]
    pub(crate) struct Observation {
        pub(crate) reader_wait_observed: AtomicBool,
        pub(crate) recovery_samples: Mutex<Vec<[u64; 4]>>,
        pub(crate) coordinator_waits: AtomicUsize,
        pub(crate) panic_before_collection: AtomicBool,
        pub(crate) sleeps: AtomicUsize,
        pub(crate) checks: AtomicUsize,
        pub(crate) initial_waits: AtomicUsize,
        pub(crate) sweep_waiting: AtomicBool,
        pub(crate) election_waiting: AtomicBool,
        hooks: Mutex<HashMap<ThreadId, fn()>>,
    }
    impl Observation {
        fn replace_hook(&self, thread: ThreadId, hook: Option<fn()>) -> Option<fn()> {
            let mut hooks = self.hooks.lock().unwrap();
            match hook {
                Some(hook) => hooks.insert(thread, hook),
                None => hooks.remove(&thread),
            }
        }
        pub(crate) fn set_initial_hook(&self, hook: Option<fn()>) {
            self.replace_hook(current().id(), hook);
        }
        pub(crate) fn initial_hook(&self, hook: fn()) -> HookScope<'_> {
            let thread = current().id();
            HookScope {
                owner: self,
                thread,
                previous: self.replace_hook(thread, Some(hook)),
            }
        }
    }
    pub(crate) struct HookScope<'a> {
        owner: &'a Observation,
        thread: ThreadId,
        previous: Option<fn()>,
    }
    impl Drop for HookScope<'_> {
        fn drop(&mut self) {
            self.owner.replace_hook(self.thread, self.previous);
        }
    }
    impl WaitObservation for Observation {
        fn reader_wait<'a>(owner: impl FnOnce() -> (&'a Self, bool)) {
            let (owner, active) = owner();
            if active {
                owner.reader_wait_observed.store(true, Ordering::Release);
            }
        }
        fn recovery_sample<'a>(owner: impl FnOnce() -> (&'a Self, [u64; 4])) {
            let (owner, sample) = owner();
            owner.recovery_samples.lock().unwrap().push(sample);
        }

        fn coordinator_wait<'a>(owner: impl FnOnce() -> &'a Self) {
            owner().coordinator_waits.fetch_add(1, Ordering::Release);
        }
        fn before_coordinator_collection<'a>(owner: impl FnOnce() -> &'a Self) {
            assert!(
                !owner().panic_before_collection.load(Ordering::Relaxed),
                "injected coordinator collection panic"
            );
        }

        fn checked<'a>(owner: impl FnOnce() -> &'a Self) {
            owner().checks.fetch_add(1, Ordering::Relaxed);
        }
        fn initial_wait<'a>(owner: impl FnOnce() -> &'a Self) {
            owner().initial_waits.fetch_add(1, Ordering::Release);
        }
        fn sleeping<'a>(owner: impl FnOnce() -> (&'a Self, bool)) {
            let (owner, sweeping) = owner();
            if sweeping {
                owner.sweep_waiting.store(true, Ordering::Release);
            } else {
                owner.election_waiting.store(true, Ordering::Release);
            }
            owner.sleeps.fetch_add(1, Ordering::Release);
        }
        fn before_initial<'a>(owner: impl FnOnce() -> &'a Self) {
            let hook = owner().hooks.lock().unwrap().get(&current().id()).copied();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    #[test]
    fn wait_observation_owners_keep_counts_and_thread_hooks_independent() {
        let first = Observation::default();
        let second = Observation::default();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let a = scope.spawn(crate::runtime::serving(|| {
                let _hook = first.initial_hook(|| panic!("hook ran on another thread"));
                for _ in 0..64 {
                    Observation::checked(|| &first);
                    Observation::sleeping(|| (&first, true));
                    barrier.wait();
                    barrier.wait();
                }
            }));
            let b = scope.spawn(crate::runtime::serving(|| {
                for _ in 0..64 {
                    barrier.wait();
                    Observation::before_initial(|| &first);
                    Observation::initial_wait(|| &second);
                    Observation::sleeping(|| (&second, false));
                    barrier.wait();
                }
            }));
            a.join().unwrap();
            b.join().unwrap();
        });
        assert_eq!(first.checks.load(Ordering::Relaxed), 64);
        assert_eq!(first.initial_waits.load(Ordering::Relaxed), 0);
        assert_eq!(second.checks.load(Ordering::Relaxed), 0);
        assert_eq!(second.initial_waits.load(Ordering::Relaxed), 64);
        for owner in [&first, &second] {
            assert_eq!(owner.sleeps.load(Ordering::Relaxed), 64);
        }
        assert!(first.sweep_waiting.load(Ordering::Relaxed));
        assert!(!second.sweep_waiting.load(Ordering::Relaxed));
        assert!(!first.election_waiting.load(Ordering::Relaxed));
        assert!(second.election_waiting.load(Ordering::Relaxed));
        assert!(first.hooks.lock().unwrap().is_empty());
        let panic = std::panic::catch_unwind(|| {
            let _outer = first.initial_hook(|| {});
            {
                let _inner = first.initial_hook(|| panic!("scoped hook unwind"));
                Observation::before_initial(|| &first);
            }
        });
        assert!(panic.is_err());
        assert!(first.hooks.lock().unwrap().is_empty());
        Observation::before_initial(|| &first);
        first.panic_before_collection.store(true, Ordering::Relaxed);
        Observation::before_coordinator_collection(|| &second);
        assert!(
            std::panic::catch_unwind(|| {
                Observation::before_coordinator_collection(|| &first);
            })
            .is_err()
        );
        Observation::coordinator_wait(|| &first);
        assert_eq!(first.coordinator_waits.load(Ordering::Acquire), 1);
        assert_eq!(second.coordinator_waits.load(Ordering::Acquire), 0);
        Observation::reader_wait(|| (&first, true));
        Observation::reader_wait(|| (&second, false));
        assert!(first.reader_wait_observed.load(Ordering::Acquire));
        assert!(!second.reader_wait_observed.load(Ordering::Acquire));
        Observation::recovery_sample(|| (&first, [1, 2, 3, 4]));
        assert_eq!(*first.recovery_samples.lock().unwrap(), vec![[1, 2, 3, 4]]);
        assert!(second.recovery_samples.lock().unwrap().is_empty());
        <()>::reader_wait(|| panic!("disabled reader inspection"));
        <()>::recovery_sample(|| panic!("disabled budget inspection"));
        <()>::coordinator_wait(|| panic!("disabled lookup"));
        <()>::before_coordinator_collection(|| panic!("disabled lookup"));
        <()>::checked(|| panic!("disabled lookup"));
        <()>::sleeping(|| panic!("disabled heap inspection"));
        <()>::initial_wait(|| panic!("disabled lookup"));
        <()>::before_initial(|| panic!("disabled lookup"));
    }
}
