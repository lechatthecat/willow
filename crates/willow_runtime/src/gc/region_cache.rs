//! Retained storage of released old regions (willow-8hq4.47).
//!
//! Releasing an empty region through `dealloc` lets the system allocator trim
//! it back to the OS, so the next region of the same size refaults every page.
//! Whether that happens depends on unrelated allocations (glibc trims the top
//! of its brk heap), making steady churn cost O(allocated region bytes) page
//! faults. Sweep instead parks released storage here:
//!
//! - Blocks are bucketed by exact capacity (regular regions share one size,
//!   large-object regions repeat for deterministic growth sequences). A reused
//!   region starts non-pristine, so every allocation re-zeroes its span.
//! - Retained bytes are bounded by [`GC_REGION_CACHE_MAX_BYTES`] and count
//!   toward the hard memory limit; reservations evict them before they could
//!   be refused (see `can_reserve`).
//! - Each sweep start detaches the blocks that stayed unused for
//!   [`GC_REGION_CACHE_IDLE_SWEEPS`] sweeps and frees them outside the heap
//!   lock. The age tolerates cycle-to-cycle demand that alternates by a few
//!   regions; freeing after one idle cycle would refault those every other
//!   cycle.

use super::*;
use std::collections::VecDeque;

/// Upper bound on retained released-region storage.
pub(super) const GC_REGION_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Sweeps a retained block may stay unused before it is freed.
pub(super) const GC_REGION_CACHE_IDLE_SWEEPS: u64 = 4;

#[derive(Default)]
pub(super) struct RegionStorageCache {
    /// Capacity -> (block, sweep generation when retained), oldest first.
    blocks: HashMap<usize, VecDeque<(*mut u8, u64)>>,
    retained_bytes: usize,
    /// Sweeps started so far.
    generation: u64,
}

impl RegionStorageCache {
    pub(super) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// The most recently retained block of exactly `capacity` bytes. Its
    /// contents are stale. O(1) expected.
    pub(super) fn take(&mut self, capacity: usize) -> Option<*mut u8> {
        let bucket = self.blocks.get_mut(&capacity)?;
        let (base, _) = bucket.pop_back()?;
        if bucket.is_empty() {
            self.blocks.remove(&capacity);
        }
        self.retained_bytes -= capacity;
        Some(base)
    }

    /// Retain `base` if `budget` allows; otherwise hand it back to be freed.
    /// O(1) expected.
    ///
    /// # Safety
    /// `base` came from [`allocate_region_storage`] with `capacity` bytes, is
    /// owned by the caller and is not referenced by any heap index.
    pub(super) unsafe fn retain(
        &mut self,
        base: *mut u8,
        capacity: usize,
        budget: usize,
    ) -> Option<*mut u8> {
        if self.retained_bytes.saturating_add(capacity) > budget.min(GC_REGION_CACHE_MAX_BYTES) {
            return Some(base);
        }
        self.blocks
            .entry(capacity)
            .or_default()
            .push_back((base, self.generation));
        self.retained_bytes += capacity;
        None
    }

    /// Start a sweep: detach the blocks idle for [`GC_REGION_CACHE_IDLE_SWEEPS`]
    /// sweeps so the caller frees them without the heap lock.
    /// O(capacity buckets + detached blocks).
    pub(super) fn begin_sweep(&mut self) -> StaleStorage {
        self.generation += 1;
        let mut stale = Vec::new();
        let retained = &mut self.retained_bytes;
        let generation = self.generation;
        self.blocks.retain(|&capacity, bucket| {
            while let Some(&(base, retained_at)) = bucket.front()
                && generation - retained_at > GC_REGION_CACHE_IDLE_SWEEPS
            {
                bucket.pop_front();
                *retained -= capacity;
                stale.push((base, capacity));
            }
            !bucket.is_empty()
        });
        StaleStorage(stale)
    }

    /// Free retained blocks, oldest first per capacity, until at most `keep`
    /// bytes remain. O(1) when nothing is freed, else O(capacity buckets +
    /// freed blocks).
    pub(super) fn shrink_to(&mut self, keep: usize) {
        if self.retained_bytes <= keep {
            return;
        }
        let retained = &mut self.retained_bytes;
        self.blocks.retain(|&capacity, bucket| {
            while *retained > keep
                && let Some((base, _)) = bucket.pop_front()
            {
                *retained -= capacity;
                // SAFETY: retained blocks are owned exclusively by the cache.
                unsafe { free_region_storage(base, capacity) };
            }
            !bucket.is_empty()
        });
    }
}

impl Drop for RegionStorageCache {
    fn drop(&mut self) {
        self.shrink_to(0);
    }
}

/// Blocks detached from the cache; dropping frees them.
#[derive(Default)]
pub(super) struct StaleStorage(Vec<(*mut u8, usize)>);

// SAFETY: the blocks are unindexed storage owned only by this value.
unsafe impl Send for StaleStorage {}

impl Drop for StaleStorage {
    fn drop(&mut self) {
        for (base, capacity) in self.0.drain(..) {
            // SAFETY: detached blocks are owned exclusively here.
            unsafe { free_region_storage(base, capacity) };
        }
    }
}

/// # Safety
/// `base` came from [`allocate_region_storage`] with `capacity` bytes and is
/// freed exactly once.
pub(super) unsafe fn free_region_storage(base: *mut u8, capacity: usize) {
    let layout = Layout::from_size_align(capacity, std::mem::align_of::<GcHeader>())
        .expect("old-region allocation layout remains valid");
    unsafe { dealloc(base, layout) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(capacity: usize) -> *mut u8 {
        OldRegion::new(RegionKind::Old, capacity)
            .expect("test region allocation must succeed")
            .take_storage()
    }

    fn retain(cache: &mut RegionStorageCache, base: *mut u8, capacity: usize) {
        // SAFETY: `base` came from `block(capacity)` and is owned here.
        assert!(unsafe { cache.retain(base, capacity, usize::MAX) }.is_none());
    }

    #[test]
    fn region_cache_take_matches_exact_capacity_newest_first() {
        let mut cache = RegionStorageCache::default();
        let (first, second, wide) = (block(256), block(256), block(512));
        retain(&mut cache, first, 256);
        retain(&mut cache, second, 256);
        retain(&mut cache, wide, 512);
        assert_eq!(cache.retained_bytes(), 1024);
        assert_eq!(cache.take(128), None);
        assert_eq!(cache.take(256), Some(second));
        assert_eq!(cache.take(256), Some(first));
        assert_eq!(cache.take(256), None);
        assert_eq!(cache.retained_bytes(), 512);
        assert!(!cache.blocks.contains_key(&256));
        assert_eq!(cache.take(512), Some(wide));
        assert_eq!(cache.retained_bytes(), 0);
        for (base, capacity) in [(first, 256), (second, 256), (wide, 512)] {
            // SAFETY: every block left the cache and is freed once.
            unsafe { free_region_storage(base, capacity) };
        }
    }

    #[test]
    fn region_cache_retain_respects_budget_and_global_cap() {
        let mut cache = RegionStorageCache::default();
        let (kept, refused) = (block(256), block(256));
        // SAFETY: both blocks are owned here; a refused block returns.
        unsafe {
            assert_eq!(cache.retain(kept, 256, 300), None);
            assert_eq!(cache.retain(refused, 256, 300), Some(refused));
            free_region_storage(refused, 256);
        }
        let huge = block(GC_REGION_CACHE_MAX_BYTES);
        // SAFETY: as above; an unlimited budget is still capped.
        unsafe {
            assert_eq!(
                cache.retain(huge, GC_REGION_CACHE_MAX_BYTES, usize::MAX),
                Some(huge)
            );
            free_region_storage(huge, GC_REGION_CACHE_MAX_BYTES);
        }
        assert_eq!(cache.retained_bytes(), 256);
    }

    #[test]
    fn region_cache_frees_only_blocks_idle_past_the_age() {
        let mut cache = RegionStorageCache::default();
        let (old, young, reused) = (block(256), block(256), block(512));
        retain(&mut cache, old, 256);
        retain(&mut cache, reused, 512);
        for _ in 0..GC_REGION_CACHE_IDLE_SWEEPS - 1 {
            assert!(cache.begin_sweep().0.is_empty());
        }
        // Taking and re-retaining restamps a block as recently used.
        assert_eq!(cache.take(512), Some(reused));
        retain(&mut cache, reused, 512);
        retain(&mut cache, young, 256);
        assert!(cache.begin_sweep().0.is_empty());
        let stale = cache.begin_sweep();
        assert_eq!(stale.0, vec![(old, 256)]);
        drop(stale);
        assert_eq!(cache.retained_bytes(), 768);
        assert_eq!(cache.take(256), Some(young));
        // SAFETY: `young` left the cache.
        unsafe { free_region_storage(young, 256) };
        for _ in 0..GC_REGION_CACHE_IDLE_SWEEPS {
            drop(cache.begin_sweep());
        }
        assert_eq!(cache.retained_bytes(), 0);
        assert!(cache.blocks.is_empty());
    }

    #[test]
    fn region_cache_shrink_frees_oldest_blocks_first() {
        let mut cache = RegionStorageCache::default();
        let blocks = [block(256), block(256), block(256)];
        for base in blocks {
            retain(&mut cache, base, 256);
        }
        cache.shrink_to(300);
        assert_eq!(cache.retained_bytes(), 256);
        assert_eq!(cache.take(256), Some(blocks[2]));
        retain(&mut cache, blocks[2], 256);
        cache.shrink_to(0);
        assert_eq!(cache.retained_bytes(), 0);
        assert!(cache.blocks.is_empty());
    }
}
