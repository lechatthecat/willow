//! Exact-key descriptor interning for runtime allocation paths. Generated
//! allocation sites instead reference immutable object-module data directly.
//!
//! Each runtime owns its registry, so independent runtimes never share
//! counts; every entry names its registry, so a release never depends on
//! which runtime the releasing thread serves.
//!
//! Runtime allocation acquires a descriptor per object from many threads, so
//! the hit path takes only the shared lock and an atomic reference increment; a single mutex
//! here serialized every runtime allocation.
//! Entries whose count reaches zero stay interned until the map doubles past
//! its size after the previous purge, so hot shapes are not reinserted under
//! the exclusive lock after every sweep and the map stays O(peak live shapes).
//! Keys are four runtime-chosen words, so the folded-multiply hasher replaces
//! SipHash, which dominated runtime allocation CPU.
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockReadGuard, TryLockError};
use willow_abi::GcLayoutDescriptor;

const MIN_PURGE_ENTRIES: usize = 1024;

/// One interned shape. The descriptor is the first field, so a header's
/// descriptor address is also its entry address, and each entry names its
/// registry: releases need neither a key lookup nor the releasing thread's
/// runtime selection.
#[repr(C)]
struct Entry {
    descriptor: GcLayoutDescriptor,
    references: AtomicUsize,
    registry: &'static RwLock<Registry>,
}

/// Boxed entry keyed by its descriptor.
struct Interned(Box<Entry>);

impl PartialEq for Interned {
    fn eq(&self, other: &Self) -> bool {
        self.0.descriptor == other.0.descriptor
    }
}
impl Eq for Interned {}
impl std::hash::Hash for Interned {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.descriptor.hash(state);
    }
}
impl std::borrow::Borrow<GcLayoutDescriptor> for Interned {
    fn borrow(&self) -> &GcLayoutDescriptor {
        &self.0.descriptor
    }
}

struct Registry {
    entries: HashSet<Interned, BuildHasherDefault<super::minor::AddressHasher>>,
    purge_at: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            entries: HashSet::default(),
            purge_at: MIN_PURGE_ENTRIES,
        }
    }
}

fn address_of(entry: &Entry) -> usize {
    (entry as *const Entry) as usize
}

/// SAFETY: `address` came from `acquire` and its reference is unreleased, so
/// the entry is not purged.
unsafe fn entry_at<'a>(address: usize) -> &'a Entry {
    unsafe { &*(address as *const Entry) }
}

impl Registry {
    /// Shared-lock hit path.
    fn acquire_existing(&self, layout: &GcLayoutDescriptor) -> Option<usize> {
        let entry = &self.entries.get(layout)?.0;
        entry.references.fetch_add(1, Ordering::Relaxed);
        Some(address_of(entry))
    }

    fn acquire(&mut self, owner: &'static RwLock<Registry>, layout: GcLayoutDescriptor) -> usize {
        if let Some(address) = self.acquire_existing(&layout) {
            return address;
        }
        if self.entries.len() >= self.purge_at {
            // The exclusive lock excludes shared-lock increments and
            // releases, so a zero count cannot be resurrected concurrently.
            // Amortized O(1).
            self.entries
                .retain(|entry| entry.0.references.load(Ordering::Relaxed) != 0);
            self.purge_at = MIN_PURGE_ENTRIES.max(self.entries.len() * 2);
        }
        let entry = Box::new(Entry {
            descriptor: layout,
            references: AtomicUsize::new(1),
            registry: owner,
        });
        let address = address_of(&entry);
        self.entries.insert(Interned(entry));
        address
    }

    fn release(&self, address: usize) {
        // SAFETY: each runtime header acquires one reference and releases it
        // exactly once, under this registry's shared lock.
        let entry = unsafe { entry_at(address) };
        let previous = entry.references.fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "GC descriptor reference underflow");
    }

    #[cfg(test)]
    fn live_entries(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.0.references.load(Ordering::Relaxed) != 0)
            .count()
    }
}

/// A runtime's descriptor registry.
#[derive(Default)]
pub(super) struct Layouts(RwLock<Registry>);

/// The registry interning this thread's runtime allocations: the serving
/// runtime's, so independent runtimes never share counts.
fn registry() -> &'static RwLock<Registry> {
    &crate::runtime::current().gc.layouts.0
}

pub(super) fn acquire(layout: GcLayoutDescriptor) -> usize {
    let owner = registry();
    if let Some(address) = owner.read().unwrap().acquire_existing(&layout) {
        return address;
    }
    owner.write().unwrap().acquire(owner, layout)
}

/// The registry that interned `address`.
fn owner_of(address: usize) -> &'static RwLock<Registry> {
    // SAFETY: the caller holds the unreleased reference behind `address`.
    unsafe { entry_at(address) }.registry
}

pub(super) fn release(address: usize) {
    owner_of(address).read().unwrap().release(address);
}

/// Pins the interning registry during one reclamation without waiting behind
/// a purge. The object reference remains live until release() is called
/// under this guard.
pub(super) struct ReleaseGuard(RwLockReadGuard<'static, Registry>);
impl ReleaseGuard {
    pub(super) fn release(&self, address: usize) {
        self.0.release(address);
    }
}
pub(super) fn try_release_guard(address: usize) -> Option<ReleaseGuard> {
    match owner_of(address).try_read() {
        Ok(guard) => Some(ReleaseGuard(guard)),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(_)) => panic!("GC descriptor registry poisoned"),
    }
}

#[cfg(test)]
pub(super) fn with_exclusive_registry(action: impl FnOnce()) {
    let _guard = registry().write().unwrap();
    action();
}

#[cfg(test)]
pub(super) fn references(key: GcLayoutDescriptor) -> usize {
    registry()
        .read()
        .unwrap()
        .entries
        .get(&key)
        .map_or(0, |entry| entry.0.references.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_size_does_not_truncate_large_allocations() {
        let owner: &'static RwLock<Registry> = Box::leak(Box::default());
        let mut registry = owner.write().unwrap();
        let key = GcLayoutDescriptor {
            type_id: 0,
            layout_id: 0,
            gc_ref_mask: 0,
            size: u64::from(u32::MAX) + 1,
        };
        let address = registry.acquire(owner, key);
        assert_eq!(unsafe { *(address as *const GcLayoutDescriptor) }, key);
        registry.release(address);
        assert_eq!(registry.live_entries(), 0);
    }

    #[test]
    fn descriptors_share_exact_shapes_and_release_unique_sizes() {
        for n in [1, 16, 256, 4096] {
            let owner: &'static RwLock<Registry> = Box::leak(Box::default());
            let mut registry = owner.write().unwrap();
            let mut addresses = Vec::new();
            for size in 1..=n {
                // Equal fingerprints must not merge distinct allocation sizes.
                let layout = GcLayoutDescriptor {
                    type_id: 7,
                    layout_id: 1,
                    gc_ref_mask: 2,
                    size,
                };
                let first = registry.acquire(owner, layout);
                assert_eq!(first, registry.acquire(owner, layout));
                addresses.push(first);
            }
            assert_eq!(registry.entries.len(), n as usize);
            for address in addresses {
                registry.release(address);
                assert_eq!(
                    unsafe { (*(address as *const GcLayoutDescriptor)).type_id },
                    7
                );
                registry.release(address);
            }
            assert_eq!(registry.live_entries(), 0);
            println!(
                "shapes={n} acquisitions={} releases={} remaining=0",
                n * 2,
                n * 2
            );
        }
    }

    #[test]
    fn released_shapes_stay_interned_until_the_map_doubles() {
        let shape = |size| GcLayoutDescriptor {
            type_id: 3,
            layout_id: 4,
            gc_ref_mask: 0,
            size,
        };
        let owner: &'static RwLock<Registry> = Box::leak(Box::default());
        let mut registry = owner.write().unwrap();
        // A hot shape released to zero and reacquired keeps its entry.
        let hot = registry.acquire(owner, shape(1));
        registry.release(hot);
        assert_eq!(registry.acquire(owner, shape(1)), hot);
        registry.release(hot);
        for size in 2..=MIN_PURGE_ENTRIES as u64 {
            let address = registry.acquire(owner, shape(size));
            registry.release(address);
        }
        assert_eq!(registry.entries.len(), MIN_PURGE_ENTRIES);
        // The next insertion purges every zero-count entry once.
        let live = registry.acquire(owner, shape(0));
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.live_entries(), 1);
        assert_eq!(registry.purge_at, MIN_PURGE_ENTRIES);
        registry.release(live);
    }
}
