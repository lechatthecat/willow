//! Capacity accounting for native storage owned by runtime objects.
//!
//! Guards reconcile once per mutation, without walking container contents.
//! No collection or safepoint is permitted while a native owner is locked.
//! The hard cap is checked before publishing the operation's new footprint;
//! a Rust allocation can transiently exist before that check (allocator metadata
//! and temporary/reallocation overlap are not part of this retained-byte gauge).
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult};

pub(crate) trait Footprint {
    fn native_bytes(&mut self) -> usize;
}

/// Conservative SwissTable reservation, including empty buckets/control bytes.
/// std exposes usable capacity, not its allocation layout. Keep the high-water
/// charge until owner destruction: removals/tombstones can reduce capacity()
/// without releasing the allocation. These owners never shrink their tables.
pub(crate) fn hash_bytes<K, V>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    capacity
        .checked_next_power_of_two()
        .and_then(|buckets| buckets.checked_mul(size_of::<(K, V)>() + 1))
        .and_then(|bytes| bytes.checked_add(64 + align_of::<(K, V)>()))
        .unwrap_or_else(|| {
            crate::failure::resource_exhausted(format_args!("native capacity overflow"))
        })
}

#[derive(Debug)]
struct Owner<T: Footprint> {
    // Fields drop in declaration order: free the value before its charge.
    value: T,
    base: usize,
    charge: Charge,
}
impl<T: Footprint> Owner<T> {
    fn reconcile(&mut self) {
        let bytes = self
            .base
            .checked_add(self.value.native_bytes())
            .unwrap_or_else(|| {
                crate::failure::resource_exhausted(format_args!("native capacity overflow"))
            });
        // Owned containers retain capacity on removal. A high water mark also
        // avoids undercounting std HashMap's tombstone-dependent capacity().
        if bytes > self.charge.0 {
            crate::gc::charge_external(bytes - self.charge.0);
            self.charge.0 = bytes;
        }
    }
}
#[derive(Debug)]
struct Charge(usize);
impl Drop for Charge {
    fn drop(&mut self) {
        crate::gc::release_external(self.0);
    }
}

#[derive(Debug)]
pub(crate) struct Mutex<T: Footprint>(std::sync::Mutex<Owner<T>>);
impl<T: Footprint> Mutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self::with_base(value, 0)
    }
    pub(crate) fn with_base(value: T, base: usize) -> Self {
        let mut owner = Owner {
            value,
            base,
            charge: Charge(0),
        };
        owner.reconcile();
        Self(std::sync::Mutex::new(owner))
    }
    #[cfg(test)]
    pub(crate) fn is_poisoned(&self) -> bool {
        self.0.is_poisoned()
    }
    pub(crate) fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        self.0
            .lock()
            .map(MutexGuard)
            .map_err(|p| PoisonError::new(MutexGuard(p.into_inner())))
    }
    pub(crate) fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        self.0.try_lock().map(MutexGuard).map_err(|e| match e {
            TryLockError::WouldBlock => TryLockError::WouldBlock,
            TryLockError::Poisoned(p) => {
                TryLockError::Poisoned(PoisonError::new(MutexGuard(p.into_inner())))
            }
        })
    }
}
pub(crate) struct MutexGuard<'a, T: Footprint>(std::sync::MutexGuard<'a, Owner<T>>);
impl<T: Footprint> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.value
    }
}
impl<T: Footprint> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0.value
    }
}
impl<T: Footprint> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.0.reconcile();
    }
}

/// Per-table high water marks prevent one table's tombstones from hiding
/// growth in a different table in the same owner. No table scanning is needed.
pub(crate) fn retain_capacity(slot: &mut usize, current: usize) -> usize {
    *slot = (*slot).max(current);
    *slot
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Buffer {
        values: Vec<u64>,
        visits: Arc<AtomicUsize>,
    }
    impl Footprint for Buffer {
        fn native_bytes(&mut self) -> usize {
            self.visits.fetch_add(1, Ordering::Relaxed);
            self.values.capacity() * size_of::<u64>()
        }
    }

    #[test]
    fn accounting_visits_once_per_guard_independent_of_buffer_length() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let baseline = crate::gc_telemetry::external_bytes();
        for count in [1, 32, 1024] {
            let visits = Arc::new(AtomicUsize::new(0));
            let buffer = Mutex::new(Buffer {
                values: Vec::new(),
                visits: visits.clone(),
            });
            for value in 0..count {
                buffer.lock().unwrap().values.push(value);
            }
            assert_eq!(visits.load(Ordering::Relaxed), count as usize + 1);
            let grown = crate::gc_telemetry::external_bytes();
            for _ in 0..count {
                buffer.lock().unwrap().values[0] = 42;
            }
            assert_eq!(crate::gc_telemetry::external_bytes(), grown);
            assert_eq!(visits.load(Ordering::Relaxed), 2 * count as usize + 1);
            println!(
                "native accounting mutations={} footprint_visits={}",
                2 * count,
                visits.load(Ordering::Relaxed)
            );
            drop(buffer);
            assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
        }
    }

    #[test]
    fn poisoned_guard_accounts_growth_and_drop_releases_it() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let baseline = crate::gc_telemetry::external_bytes();
        let buffer = Mutex::new(Buffer {
            values: Vec::new(),
            visits: Arc::new(AtomicUsize::new(0)),
        });
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut guard = buffer.lock().unwrap();
                guard.values.resize(128, 0);
                panic!("injected after native growth");
            }))
            .is_err()
        );
        assert!(buffer.is_poisoned());
        assert!(crate::gc_telemetry::external_bytes() >= baseline + 1024);
        drop(buffer);
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
    }

    #[test]
    fn parallel_owners_reconcile_without_lost_updates() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let baseline = crate::gc_telemetry::external_bytes();
        let owners: Vec<_> = (0..8)
            .map(|_| {
                Arc::new(Mutex::new(Buffer {
                    values: Vec::new(),
                    visits: Arc::new(AtomicUsize::new(0)),
                }))
            })
            .collect();
        std::thread::scope(|scope| {
            for owner in &owners {
                scope.spawn(move || {
                    for value in 0..1024 {
                        owner.lock().unwrap().values.push(value);
                    }
                });
            }
        });
        assert_eq!(
            crate::gc_telemetry::external_bytes() - baseline,
            8 * 1024 * 8
        );
        drop(owners);
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
    }

    #[test]
    fn native_growth_enforces_hard_limit_without_managed_allocation() {
        const KEY: &str = "WILLOW_NATIVE_LIMIT_TEST";
        if let Ok(kind) = std::env::var(KEY) {
            crate::gc::willow_gc_init();
            if kind == "map" {
                let map = crate::map::willow_map_new(0, 0, 0);
                for key in 0..100_000 {
                    crate::map::willow_map_insert(map, key, 0, key, 0);
                }
            } else {
                let channel = crate::channel::willow_channel_new(0);
                for value in 0..100_000 {
                    crate::channel::willow_channel_send_i64(channel, value);
                }
            }
            panic!("native growth bypassed memory limit");
        }
        for kind in ["map", "channel"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "native_memory::tests::native_growth_enforces_hard_limit_without_managed_allocation", "--nocapture"])
                .env(KEY, kind).env("WILLOW_GC_MEMORY_LIMIT", "524288")
                .output().unwrap();
            assert_eq!(result.status.code(), Some(1));
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(stderr.contains("GC memory limit exceeded"), "{stderr}");
            assert!(
                stderr.contains("managed and native reservations"),
                "{stderr}"
            );
        }
    }
}
