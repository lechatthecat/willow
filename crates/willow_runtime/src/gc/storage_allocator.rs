//! Allocator ownership shared by collector storage and outside-lock TLAB preparation.
use std::alloc::{Layout, alloc, alloc_zeroed};

pub(super) trait StorageAllocation: Default {
    fn share(&self) -> Self;
    /// # Safety
    /// The layout is nonzero. A successful allocation must be freed once with
    /// the same layout, after all objects and metadata views stop using it.
    unsafe fn allocate<const ZEROED: bool>(&self, layout: Layout) -> *mut u8;
}

#[derive(Clone, Copy, Default)]
pub(super) struct SystemStorage {
    _private: (),
}
impl StorageAllocation for SystemStorage {
    fn share(&self) -> Self {
        *self
    }
    unsafe fn allocate<const ZEROED: bool>(&self, layout: Layout) -> *mut u8 {
        if ZEROED {
            unsafe { alloc_zeroed(layout) }
        } else {
            unsafe { alloc(layout) }
        }
    }
}
#[cfg(not(test))]
pub(super) type RuntimeStorageAllocator = SystemStorage;
#[cfg(test)]
pub(super) use tests::RuntimeStorageAllocator;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::thread::{ThreadId, current};

    #[derive(Default)]
    struct Counts {
        failures: usize,
        attempts: usize,
        zeroed_bytes: usize,
    }
    #[derive(Default)]
    pub(crate) struct RuntimeStorageAllocator(Arc<Mutex<HashMap<ThreadId, Counts>>>);
    impl RuntimeStorageAllocator {
        fn with_current<R>(&self, f: impl FnOnce(&mut Counts) -> R) -> R {
            f(self.0.lock().unwrap().entry(current().id()).or_default())
        }
        pub(crate) fn failures(&self) -> usize {
            self.with_current(|v| v.failures)
        }
        pub(crate) fn attempts(&self) -> usize {
            self.with_current(|v| v.attempts)
        }
        pub(crate) fn zeroed_bytes(&self) -> usize {
            self.with_current(|v| v.zeroed_bytes)
        }
        pub(crate) fn set_failures(&self, n: usize) {
            self.with_current(|v| v.failures = n);
        }
        pub(crate) fn set_attempts(&self, n: usize) {
            self.with_current(|v| v.attempts = n);
        }
        pub(crate) fn set_zeroed_bytes(&self, n: usize) {
            self.with_current(|v| v.zeroed_bytes = n);
        }
    }
    impl StorageAllocation for RuntimeStorageAllocator {
        fn share(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
        unsafe fn allocate<const ZEROED: bool>(&self, layout: Layout) -> *mut u8 {
            let fail = self.with_current(|v| {
                v.attempts += 1;
                if v.failures > 0 {
                    v.failures -= 1;
                    true
                } else {
                    if ZEROED {
                        v.zeroed_bytes += layout.size();
                    }
                    false
                }
            });
            if fail {
                return std::ptr::null_mut();
            }
            unsafe { SystemStorage::default().allocate::<ZEROED>(layout) }
        }
    }

    #[test]
    fn storage_allocator_failures_and_counts_are_owner_and_thread_local() {
        let first = RuntimeStorageAllocator::default();
        let shared = first.share();
        let second = RuntimeStorageAllocator::default();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [1, 3]
                .into_iter()
                .map(|stride| {
                    let (first, shared, second, barrier) = (&first, &shared, &second, &barrier);
                    scope.spawn(crate::runtime::serving(move || {
                        let layout = Layout::from_size_align(32 * stride, 16).unwrap();
                        let mut correct = true;
                        for round in 1..=64 {
                            first.set_failures(1);
                            barrier.wait();
                            let failed = unsafe { shared.allocate::<true>(layout) };
                            correct &= failed.is_null();
                            if !failed.is_null() {
                                unsafe {
                                    std::alloc::dealloc(failed, layout);
                                }
                            }
                            let actual = unsafe { second.allocate::<true>(layout) };
                            correct &= !actual.is_null();
                            if !actual.is_null() {
                                correct &=
                                    unsafe { std::slice::from_raw_parts(actual, layout.size()) }
                                        .iter()
                                        .all(|b| *b == 0);
                                unsafe {
                                    std::alloc::dealloc(actual, layout);
                                }
                            }
                            correct &= first.attempts() == round && first.failures() == 0;
                            correct &= first.zeroed_bytes() == 0;
                            correct &= second.attempts() == round
                                && second.zeroed_bytes() == round * layout.size();
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
        assert_eq!(first.attempts(), 0);
        assert_eq!(second.attempts(), 0);
    }
    #[test]
    fn gc_storage_owner_is_shared_with_heap_and_prepared_chunks() {
        use super::super::{GcRuntime, OldRegion, RegionKind, tlab};
        let first = GcRuntime::default();
        let second = GcRuntime::default();
        first.storage_allocator.set_failures(1);
        let first_prepared = {
            let state = first.heap.lock().unwrap();
            assert!(OldRegion::new(&state.storage_allocator, RegionKind::Old, 4096).is_none());
            state.storage_allocator.share()
        };
        let (region, second_prepared) = {
            let state = second.heap.lock().unwrap();
            (
                OldRegion::new(&state.storage_allocator, RegionKind::Old, 4096).unwrap(),
                state.storage_allocator.share(),
            )
        };
        assert_eq!(first.storage_allocator.failures(), 0);
        first.storage_allocator.set_failures(1);
        assert!(tlab::ChunkStorage::allocate(&first_prepared).is_none());
        let chunk = tlab::ChunkStorage::allocate(&second_prepared).unwrap();
        assert_eq!(first.storage_allocator.attempts(), 2);
        assert_eq!(first.storage_allocator.zeroed_bytes(), 0);
        assert_eq!(second.storage_allocator.attempts(), 2);
        assert_eq!(
            second.storage_allocator.zeroed_bytes(),
            4096 + super::super::GC_TLAB_CHUNK_SIZE
        );
        drop(first);
        // Prepared ownership and storage remain valid after their collector
        // container is gone; storage itself is freed by its owning value.
        first_prepared.set_failures(0);
        let detached = tlab::ChunkStorage::allocate(&first_prepared).unwrap();
        drop((detached, chunk, region, second));
    }
}
