//! Capacity accounting for native storage owned by runtime objects.
//!
//! Guards reconcile once per mutation, without walking container contents.
//! No collection or safepoint is permitted while a native owner is locked.
//! The hard cap is checked before publishing the operation's new footprint;
//! a Rust allocation can transiently exist before that check (allocator metadata
//! and temporary/reallocation overlap are not part of this retained-byte gauge).
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult};

#[cfg(test)]
thread_local! {
    static RECONCILIATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) trait Footprint {
    fn native_bytes(&mut self) -> usize;
}

/// Sum the fixed number of components in an owner's estimate without wrapping.
/// Real buffers are bounded by address space; conservative hash estimates can
/// still overflow when combined, so fail before publishing an invalid charge.
pub(crate) fn sum_bytes<const N: usize>(parts: [usize; N]) -> usize {
    parts
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .unwrap_or_else(|| {
            crate::failure::resource_exhausted(format_args!("native capacity overflow"))
        })
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
        #[cfg(test)]
        RECONCILIATIONS.set(RECONCILIATIONS.get() + 1);
        let bytes = self
            .base
            .checked_add(self.value.native_bytes())
            .unwrap_or_else(|| {
                crate::failure::resource_exhausted(format_args!("native capacity overflow"))
            });
        // Hash owners retain per-table high water marks in their Footprint;
        // exact buffers can release their charge after a real shrink.
        if bytes > self.charge.0 {
            crate::gc::charge_external(bytes - self.charge.0);
        } else if bytes < self.charge.0 {
            crate::gc::release_external(self.charge.0 - bytes);
        }
        self.charge.0 = bytes;
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

/// Native accounting for a compiler-proven confined owner. Callers must keep
/// concurrent GC away from the contents and disable preemption while borrowed.
pub(crate) struct Local<T: Footprint>(std::cell::RefCell<Owner<T>>);
impl<T: Footprint> Local<T> {
    pub(crate) fn new(value: T) -> Self {
        let mut owner = Owner {
            value,
            base: 0,
            charge: Charge(0),
        };
        owner.reconcile();
        Self(std::cell::RefCell::new(owner))
    }
    pub(crate) fn access(&self) -> LocalGuard<'_, T> {
        LocalGuard(self.0.borrow_mut(), false)
    }
}
pub(crate) struct LocalGuard<'a, T: Footprint>(std::cell::RefMut<'a, Owner<T>>, bool);
impl<T: Footprint> Deref for LocalGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.value
    }
}
impl<T: Footprint> DerefMut for LocalGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.1 = true;
        &mut self.0.value
    }
}
impl<T: Footprint> Drop for LocalGuard<'_, T> {
    fn drop(&mut self) {
        if self.1 {
            self.0.reconcile();
        }
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
    fn capacity_arithmetic_rejects_overflow_without_wrapping() {
        const KEY: &str = "WILLOW_NATIVE_OVERFLOW_TEST";
        if let Ok(kind) = std::env::var(KEY) {
            if kind == "sum" {
                sum_bytes([usize::MAX, 1]);
            } else {
                hash_bytes::<u64, u64>(usize::MAX);
            }
            panic!("overflow accepted");
        }
        assert_eq!(sum_bytes([usize::MAX - 1, 1]), usize::MAX);
        assert_eq!(hash_bytes::<u64, u64>(0), 0);
        for kind in ["sum", "hash"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native_memory::tests::capacity_arithmetic_rejects_overflow_without_wrapping",
                    "--nocapture",
                ])
                .env(KEY, kind)
                .output()
                .unwrap();
            assert_eq!(result.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&result.stderr).contains("native capacity overflow"));
        }
    }

    #[test]
    fn shrink_and_unwind_release_only_freed_capacity() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let baseline = crate::gc_telemetry::external_bytes();
        let buffer = Mutex::with_base(
            Buffer {
                values: vec![0; 128],
                visits: Arc::new(AtomicUsize::new(0)),
            },
            24,
        );
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline + 24 + 1024);
        buffer.lock().unwrap().values.clear();
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline + 24 + 1024);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut guard = buffer.lock().unwrap();
                guard.values.shrink_to_fit();
                panic!("injected after shrink");
            }))
            .is_err()
        );
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline + 24);
        // A live native owner is not zeroed by GC reset; its later Drop balances.
        crate::gc::willow_gc_init();
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline + 24);
        drop(buffer);
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
    }

    #[test]
    fn increasing_channel_counts_have_linear_accounting_and_balanced_lifetimes() {
        let _guard = crate::gc::runtime_test_guard();
        for reset in [false, true] {
            for count in [1, 32, 1024] {
                crate::gc::willow_gc_init();
                let baseline = crate::gc_telemetry::external_bytes();
                RECONCILIATIONS.set(0);
                let mut roots = Vec::new();
                let mut per_channel = 0;
                for _ in 0..count {
                    let channel = crate::channel::willow_channel_new(0);
                    roots.push(crate::gc::GcRootHandle::new(channel.cast()));
                    crate::channel::willow_channel_send_i64(channel, 42);
                    if per_channel == 0 {
                        per_channel = crate::gc_telemetry::external_bytes() - baseline;
                    }
                }
                assert!(per_channel > 0);
                assert_eq!(
                    crate::gc_telemetry::external_bytes() - baseline,
                    count as u64 * per_channel
                );
                // Construction and one send each: no scan over prior channels.
                assert_eq!(RECONCILIATIONS.get(), 2 * count);
                println!(
                    "native channels={count} reconciliations={} requested_bytes={} reset={reset}",
                    RECONCILIATIONS.get(),
                    count as u64 * per_channel
                );
                drop(roots);
                if reset {
                    crate::gc::willow_gc_init();
                } else {
                    crate::gc::willow_gc_collect();
                }
                assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
            }
        }
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
