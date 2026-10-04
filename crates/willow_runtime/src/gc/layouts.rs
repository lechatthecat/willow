//! Exact-key descriptor interning for runtime allocation paths. Generated
//! allocation sites instead reference immutable object-module data directly.
//!
//! Runtime allocation acquires a descriptor per object from many threads, so
//! the hit path takes only the shared lock and an atomic reference increment
//! (willow-jz15.53); a single mutex here serialized every runtime allocation.
//! Entries whose count reaches zero stay interned until the map doubles past
//! its size after the previous purge, so hot shapes are not reinserted under
//! the exclusive lock after every sweep and the map stays O(peak live shapes).
//! Keys are four runtime-chosen words, so the folded-multiply hasher replaces
//! SipHash, which dominated runtime allocation CPU.
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock};
use willow_abi::GcLayoutDescriptor;

const MIN_PURGE_ENTRIES: usize = 1024;

struct Registry {
    entries: HashMap<
        Box<GcLayoutDescriptor>,
        AtomicUsize,
        BuildHasherDefault<super::minor::AddressHasher>,
    >,
    purge_at: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            entries: HashMap::default(),
            purge_at: MIN_PURGE_ENTRIES,
        }
    }
}

fn address_of(descriptor: &GcLayoutDescriptor) -> usize {
    (descriptor as *const GcLayoutDescriptor) as usize
}

impl Registry {
    /// Shared-lock hit path.
    fn acquire_existing(&self, layout: &GcLayoutDescriptor) -> Option<usize> {
        let (descriptor, references) = self.entries.get_key_value(layout)?;
        references.fetch_add(1, Ordering::Relaxed);
        Some(address_of(descriptor))
    }

    fn acquire(&mut self, layout: GcLayoutDescriptor) -> usize {
        if let Some(address) = self.acquire_existing(&layout) {
            return address;
        }
        if self.entries.len() >= self.purge_at {
            // The exclusive lock excludes shared-lock increments, so a zero
            // count cannot be resurrected concurrently. Amortized O(1).
            self.entries
                .retain(|_, references| *references.get_mut() != 0);
            self.purge_at = MIN_PURGE_ENTRIES.max(self.entries.len() * 2);
        }
        let descriptor = Box::new(layout);
        let address = address_of(&descriptor);
        self.entries.insert(descriptor, AtomicUsize::new(1));
        address
    }

    fn release(&self, address: usize) {
        // SAFETY: each runtime header acquires one reference and releases it
        // exactly once; a referenced entry is never purged, so its boxed key
        // outlives this read.
        let key = unsafe { *(address as *const GcLayoutDescriptor) };
        let references = self.entries.get(&key).expect("live GC descriptor");
        let previous = references.fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "GC descriptor reference underflow");
    }

    #[cfg(test)]
    fn live_entries(&self) -> usize {
        self.entries
            .values()
            .filter(|references| references.load(Ordering::Relaxed) != 0)
            .count()
    }
}

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Registry::default()))
}

pub(super) fn acquire(layout: GcLayoutDescriptor) -> usize {
    if let Some(address) = registry().read().unwrap().acquire_existing(&layout) {
        return address;
    }
    registry().write().unwrap().acquire(layout)
}

pub(super) fn release(address: usize) {
    registry().read().unwrap().release(address);
}

#[cfg(test)]
pub(super) fn references(key: GcLayoutDescriptor) -> usize {
    registry()
        .read()
        .unwrap()
        .entries
        .get(&key)
        .map_or(0, |references| references.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_size_does_not_truncate_large_allocations() {
        let mut registry = Registry::default();
        let key = GcLayoutDescriptor {
            type_id: 0,
            layout_id: 0,
            gc_ref_mask: 0,
            size: u64::from(u32::MAX) + 1,
        };
        let address = registry.acquire(key);
        assert_eq!(unsafe { *(address as *const GcLayoutDescriptor) }, key);
        registry.release(address);
        assert_eq!(registry.live_entries(), 0);
    }

    #[test]
    fn descriptors_share_exact_shapes_and_release_unique_sizes() {
        for n in [1, 16, 256, 4096] {
            let mut registry = Registry::default();
            let mut addresses = Vec::new();
            for size in 1..=n {
                // Equal fingerprints must not merge distinct allocation sizes.
                let layout = GcLayoutDescriptor {
                    type_id: 7,
                    layout_id: 1,
                    gc_ref_mask: 2,
                    size,
                };
                let first = registry.acquire(layout);
                assert_eq!(first, registry.acquire(layout));
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
        let mut registry = Registry::default();
        // A hot shape released to zero and reacquired keeps its entry.
        let hot = registry.acquire(shape(1));
        registry.release(hot);
        assert_eq!(registry.acquire(shape(1)), hot);
        registry.release(hot);
        for size in 2..=MIN_PURGE_ENTRIES as u64 {
            let address = registry.acquire(shape(size));
            registry.release(address);
        }
        assert_eq!(registry.entries.len(), MIN_PURGE_ENTRIES);
        // The next insertion purges every zero-count entry once.
        let live = registry.acquire(shape(0));
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.live_entries(), 1);
        assert_eq!(registry.purge_at, MIN_PURGE_ENTRIES);
        registry.release(live);
    }
}
