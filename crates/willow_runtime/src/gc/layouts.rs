//! Exact-key descriptor interning for runtime allocation paths. Generated
//! allocation sites instead reference immutable object-module data directly.
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use willow_abi::GcLayoutDescriptor;

#[derive(Default)]
struct Registry {
    entries: HashMap<Box<GcLayoutDescriptor>, Cell<usize>>,
}

impl Registry {
    fn acquire(&mut self, layout: GcLayoutDescriptor) -> usize {
        if let Some((descriptor, references)) = self.entries.get_key_value(&layout) {
            references.set(
                references
                    .get()
                    .checked_add(1)
                    .expect("GC descriptor reference overflow"),
            );
            return (&**descriptor as *const GcLayoutDescriptor) as usize;
        }
        let descriptor = Box::new(layout);
        let address = (&*descriptor as *const GcLayoutDescriptor) as usize;
        self.entries.insert(descriptor, Cell::new(1));
        address
    }

    fn release(&mut self, address: usize) {
        // SAFETY: each runtime header acquires one reference, released exactly
        // once while the registry mutex excludes concurrent acquire/release.
        let key = unsafe { *(address as *const GcLayoutDescriptor) };
        let references = self.entries.get(&key).expect("live GC descriptor");
        let remaining = references.get() - 1;
        references.set(remaining);
        if remaining == 0 {
            self.entries.remove(&key);
        }
    }
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

pub(super) fn acquire(layout: GcLayoutDescriptor) -> usize {
    registry().lock().unwrap().acquire(layout)
}

pub(super) fn release(address: usize) {
    registry().lock().unwrap().release(address);
}

#[cfg(test)]
pub(super) fn references(key: GcLayoutDescriptor) -> usize {
    registry()
        .lock()
        .unwrap()
        .entries
        .get(&key)
        .map_or(0, Cell::get)
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
        assert!(registry.entries.is_empty());
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
            assert!(registry.entries.is_empty());
            println!(
                "shapes={n} acquisitions={} releases={} remaining=0",
                n * 2,
                n * 2
            );
        }
    }
}
