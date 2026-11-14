//! Owner-held observations. Production specialization erases both storage and
//! observation closures; tests retain the value on the object being observed.

#[cfg(not(test))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Value<T>(std::marker::PhantomData<fn() -> T>);

#[cfg(not(test))]
impl<T> Value<T> {
    pub(crate) fn new(_: impl FnOnce() -> T) -> Self {
        Self(std::marker::PhantomData)
    }
    #[inline]
    pub(crate) fn observe(&self, _: impl FnOnce(&T)) {}
    #[inline]
    pub(crate) fn observe_mut(&mut self, _: impl FnOnce(&mut T)) {}
}

#[cfg(not(test))]
impl<T> Default for Value<T> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Value<T>(T);

#[cfg(test)]
impl<T> Value<T> {
    pub(crate) fn new(make: impl FnOnce() -> T) -> Self {
        Self(make())
    }
    pub(crate) fn observe(&self, f: impl FnOnce(&T)) {
        f(&self.0);
    }
    pub(crate) fn observe_mut(&mut self, f: impl FnOnce(&mut T)) {
        f(&mut self.0);
    }
}

#[cfg(test)]
impl<T> std::ops::Deref for Value<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

#[cfg(test)]
impl<T: PartialEq> PartialEq<T> for Value<T> {
    fn eq(&self, other: &T) -> bool {
        self.0.eq(other)
    }
}

/// Native ABI observations share the owning runtime, with thread-local views
/// for synchronous operation counts and aggregate views for background drops.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    AsyncMutexDropCount,
    AsyncRwlockDropCount,
    ChannelDropCount,
    ChannelRegistrationCount,
    ChannelReserveLocks,
    ChannelWaiterVisits,
    ChannelWakeAttempts,
    ChannelWakeBatches,
    BlockingCellDropCount,
    BlockingRwCellDropCount,
    BlockingCellRegistrationCount,
    NetworkHandleDropCount,
    WordSharedAcquires,
    WordHashCalls,
    CompactMoves,
    StringHashBytes,
    SnapshotComparisons,
    AllocationCopyBytes,
    CopiedBytes,
    ByteReads,
    JoinVisits,
    InputCopies,
    OutputCopies,
    ChannelOwnershipHashes,
    ChannelOwnershipScans,
    BuiltEvents,
}

#[cfg(not(test))]
pub(crate) struct Native;
#[cfg(not(test))]
impl Native {
    pub(crate) const fn new() -> Self {
        Self
    }
    #[inline]
    pub(crate) fn record<'a>(_: impl FnOnce() -> &'a Self, _: Event, _: usize) {}
    #[inline]
    pub(crate) fn dropped<'a>(_: impl FnOnce() -> &'a Self, _: impl FnOnce() -> Option<u32>) {}
}

#[inline]
pub(crate) fn record(event: Event, amount: usize) {
    Native::record(
        || &crate::runtime::current().gc.native_observation,
        event,
        amount,
    );
}

#[cfg(test)]
pub(crate) use native_tests::{Counter, Native};

#[cfg(test)]
mod native_tests {
    use super::Event;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex};
    const EVENTS: usize = 26;
    const AGGREGATE_EVENTS: usize = 12;

    pub(crate) struct Native {
        totals: [AtomicUsize; EVENTS],
        threads: LazyLock<Mutex<std::collections::HashMap<std::thread::ThreadId, [usize; EVENTS]>>>,
    }
    impl Native {
        pub(crate) const fn new() -> Self {
            Self {
                totals: [const { AtomicUsize::new(0) }; EVENTS],
                threads: LazyLock::new(|| Mutex::new(std::collections::HashMap::new())),
            }
        }
        pub(crate) fn record<'a>(owner: impl FnOnce() -> &'a Self, event: Event, amount: usize) {
            owner().update(event, |value| value.wrapping_add(amount));
        }
        pub(crate) fn dropped<'a>(
            owner: impl FnOnce() -> &'a Self,
            type_id: impl FnOnce() -> Option<u32>,
        ) {
            use willow_abi::runtime_type_ids::*;
            let event = match type_id() {
                Some(ASYNC_MUTEX_TYPE_ID) => Event::AsyncMutexDropCount,
                Some(ASYNC_RWLOCK_TYPE_ID) => Event::AsyncRwlockDropCount,
                Some(CHANNEL_TYPE_ID) => Event::ChannelDropCount,
                Some(BLOCKING_CELL_TYPE_ID) => Event::BlockingCellDropCount,
                Some(BLOCKING_RW_CELL_TYPE_ID) => Event::BlockingRwCellDropCount,
                Some(NETWORK_HANDLE_TYPE_ID) => Event::NetworkHandleDropCount,
                _ => return,
            };
            Self::record(owner, event, 1);
        }
        fn update(&self, event: Event, update: impl Fn(usize) -> usize) -> usize {
            let index = event as usize;
            if index < AGGREGATE_EVENTS {
                self.totals[index]
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                        Some(update(value))
                    })
                    .unwrap()
            } else {
                let mut threads = self.threads.lock().unwrap();
                let counts = threads
                    .entry(std::thread::current().id())
                    .or_insert([0; EVENTS]);
                let old = counts[index];
                counts[index] = update(old);
                old
            }
        }
        pub(crate) fn snapshot(&self, event: Event) -> usize {
            let index = event as usize;
            if index < AGGREGATE_EVENTS {
                self.totals[index].load(Ordering::SeqCst)
            } else {
                self.threads
                    .lock()
                    .unwrap()
                    .get(&std::thread::current().id())
                    .map_or(0, |counts| counts[index])
            }
        }
    }

    /// Immutable selector, not shared counter storage. Existing ABI tests read
    /// the default owner's observation; private fixtures read their own Native.
    pub(crate) struct Counter(pub(crate) Event);
    impl Counter {
        pub(crate) fn with<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
            f(self)
        }
        pub(crate) fn get(&self) -> usize {
            crate::runtime::current()
                .gc
                .native_observation
                .snapshot(self.0)
        }
        pub(crate) fn set(&self, value: usize) {
            self.replace(value);
        }
        pub(crate) fn replace(&self, value: usize) -> usize {
            crate::runtime::current()
                .gc
                .native_observation
                .update(self.0, |_| value)
        }
        pub(crate) fn load(&self, _: Ordering) -> usize {
            self.get()
        }
        pub(crate) fn store(&self, value: usize, _: Ordering) {
            self.set(value);
        }
        pub(crate) fn fetch_add(&self, value: usize, _: Ordering) -> usize {
            crate::runtime::current()
                .gc
                .native_observation
                .update(self.0, |old| old.wrapping_add(value))
        }
    }

    #[test]
    fn native_observations_separate_owners_and_thread_views() {
        let first = Native::new();
        let second = Native::new();
        std::thread::scope(|scope| {
            for amount in [3, 7] {
                let first = &first;
                let second = &second;
                scope.spawn(crate::runtime::serving(move || {
                    Native::record(|| first, Event::WordHashCalls, amount);
                    Native::record(|| second, Event::WordHashCalls, amount + 1);
                    Native::record(|| first, Event::ChannelDropCount, amount);
                    assert_eq!(first.snapshot(Event::WordHashCalls), amount);
                    assert_eq!(second.snapshot(Event::WordHashCalls), amount + 1);
                }));
            }
        });
        assert_eq!(first.snapshot(Event::ChannelDropCount), 10);
        assert_eq!(second.snapshot(Event::ChannelDropCount), 0);
        assert_eq!(first.snapshot(Event::WordHashCalls), 0);
    }
}

#[cfg(not(test))]
pub(crate) struct SchedulerHooks;
#[cfg(not(test))]
impl SchedulerHooks {
    pub(crate) const fn new() -> Self {
        Self
    }
    #[inline]
    pub(crate) fn drive<'a>(_: impl FnOnce() -> &'a crate::runtime::Runtime) {}
    #[inline]
    pub(crate) fn idle<'a>(_: impl FnOnce() -> &'a crate::runtime::Runtime, _: usize) {}
}

#[cfg(test)]
pub(crate) use scheduler_tests::SchedulerHooks;
#[cfg(test)]
mod scheduler_tests {
    use std::sync::{LazyLock, Mutex};
    type DriveHooks = std::collections::HashMap<std::thread::ThreadId, fn()>;
    type IdleHook = fn(&crate::runtime::Runtime);
    pub(crate) struct SchedulerHooks {
        drive: LazyLock<Mutex<DriveHooks>>,
        idle: Mutex<[Option<IdleHook>; 2]>,
    }
    impl SchedulerHooks {
        pub(crate) const fn new() -> Self {
            Self {
                drive: LazyLock::new(|| Mutex::new(DriveHooks::new())),
                idle: Mutex::new([None; 2]),
            }
        }
        pub(crate) fn drive<'a>(owner: impl FnOnce() -> &'a crate::runtime::Runtime) {
            let hook = owner()
                .scheduler_hooks
                .drive
                .lock()
                .unwrap()
                .remove(&std::thread::current().id());
            if let Some(hook) = hook {
                hook();
            }
        }
        pub(crate) fn with_drive_hook<R>(
            &self,
            f: impl FnOnce(&std::cell::Cell<Option<fn()>>) -> R,
        ) -> R {
            let id = std::thread::current().id();
            let slot = std::cell::Cell::new(self.drive.lock().unwrap().remove(&id));
            let result = f(&slot);
            if let Some(hook) = slot.take() {
                self.drive.lock().unwrap().insert(id, hook);
            }
            result
        }
        pub(crate) fn idle<'a>(owner: impl FnOnce() -> &'a crate::runtime::Runtime, kind: usize) {
            let owner = owner();
            let hook = owner.scheduler_hooks.idle.lock().unwrap()[kind];
            if let Some(hook) = hook {
                hook(owner);
            }
        }
        pub(crate) fn set_idle(&self, hooks: [Option<IdleHook>; 2]) {
            *self.idle.lock().unwrap() = hooks;
        }
    }
}

#[cfg(not(test))]
impl<T: Copy> Value<T> {
    pub(crate) const fn constant(_: T) -> Self {
        Self(std::marker::PhantomData)
    }
}
#[cfg(test)]
impl<T: Copy> Value<T> {
    pub(crate) const fn constant(value: T) -> Self {
        Self(value)
    }
}
