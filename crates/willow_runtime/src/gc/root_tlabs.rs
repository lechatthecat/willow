//! Live TLAB extents for address-root generation checks without the heap mutex.
//! Writers hold the heap mutex, then this lock. Readers never acquire the heap
//! mutex while holding this lock. Removal precedes storage release/reuse.
use super::*;

// An allocation is exactly one index page long, but need not be page-aligned.
// Disjoint extents therefore contribute at most two candidates to each page.
type Candidates = [Option<(usize, Arc<concurrent_bitmap::ConcurrentMarkBits>)>; 2];

#[derive(Default)]
pub(super) struct RootTlabs(std::sync::RwLock<IdMap<usize, Candidates>>);

#[cfg(test)]
thread_local! {
    static CANDIDATE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl RootTlabs {
    pub(super) fn insert(&self, chunk: &BumpChunk) {
        assert_eq!(chunk.capacity, GC_TLAB_CHUNK_SIZE);
        self.insert_view(chunk.base as usize, &chunk.mark_bitmap.bits);
    }

    fn insert_view(&self, base: usize, bits: &Arc<concurrent_bitmap::ConcurrentMarkBits>) {
        let mut pages = self.0.write().unwrap();
        for page in base / GC_TLAB_CHUNK_SIZE..=base.div_ceil(GC_TLAB_CHUNK_SIZE) {
            let candidates = pages.entry(page).or_default();
            let slot = candidates
                .iter_mut()
                .find(|slot| slot.is_none())
                .expect("disjoint fixed-size TLAB extents have at most two candidates");
            *slot = Some((base, bits.clone()));
        }
    }

    pub(super) fn remove(&self, base: usize) {
        let mut pages = self.0.write().unwrap();
        for page in base / GC_TLAB_CHUNK_SIZE..=base.div_ceil(GC_TLAB_CHUNK_SIZE) {
            let candidates = pages.get_mut(&page).expect("registered TLAB extent");
            let slot = candidates
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|(start, _)| *start == base))
                .expect("registered TLAB candidate");
            *slot = None;
            if candidates.iter().all(Option::is_none) {
                pages.remove(&page);
            }
        }
    }

    pub(super) fn clear(&self) {
        *self.0.write().unwrap() = IdMap::default();
    }

    pub(super) fn generation(&self, address: usize) -> Option<u8> {
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        let pages = self.0.read().unwrap();
        for (base, starts) in pages.get(&(header / GC_TLAB_CHUNK_SIZE))?.iter().flatten() {
            #[cfg(test)]
            CANDIDATE_VISITS.set(CANDIDATE_VISITS.get() + 1);
            let Some(offset) = header.checked_sub(*base) else {
                continue;
            };
            if offset >= GC_TLAB_CHUNK_SIZE {
                continue;
            }
            if !offset.is_multiple_of(GC_REGION_MARK_GRANULE)
                || !starts.contains(offset / GC_REGION_MARK_GRANULE)
            {
                return None;
            }
            // Start bits acquire-publish initialized headers. The extent read
            // lock prevents backing storage release. Generation changes only
            // at a stop crossed by publication's no-safepoint interval.
            let object = HeapObject::from_raw(header as *mut GcHeader).unwrap();
            return Some(object.generation());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_lookup_checks_at_most_two_candidates_with_fragmented_extents() {
        for count in [64, 256, 1024, 4096] {
            let index = RootTlabs::default();
            // Empty start maps prove membership queries never read synthetic
            // storage. Sharing these empty bits is confined to this test.
            let bits = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                GC_TLAB_CHUNK_SIZE / GC_REGION_MARK_GRANULE,
            ));
            for i in 1..=count {
                index.insert_view(i * GC_TLAB_CHUNK_SIZE + 8, &bits);
            }
            CANDIDATE_VISITS.set(0);
            for i in 1..=count {
                assert_eq!(
                    index.generation(i * GC_TLAB_CHUNK_SIZE + 8 + GC_HEADER_SIZE),
                    None
                );
                assert_eq!(
                    index.generation(i * GC_TLAB_CHUNK_SIZE + GC_HEADER_SIZE),
                    None
                );
            }
            let visits = CANDIDATE_VISITS.get();
            assert!(visits <= 4 * count);
            for i in (1..=count).step_by(2) {
                index.remove(i * GC_TLAB_CHUNK_SIZE + 8);
            }
            assert_eq!(index.0.read().unwrap().len(), count);
            eprintln!(
                "extents={count} queries={} candidate_visits={visits} live_after_removal={}",
                2 * count,
                count / 2
            );
        }
    }

    #[test]
    fn generation_index_tracks_active_retired_survivor_and_released_chunks() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let mut tls = Box::new(GcTlabState::new());
        let young = willow_gc_alloc_slow(&mut *tls, 0, 0, 8, 0);
        assert_eq!(
            runtime().root_tlabs.generation(young as usize),
            Some(GC_GENERATION_YOUNG)
        );
        assert_eq!(runtime().root_tlabs.generation(young as usize + 1), None);
        assert_eq!(runtime().root_tlabs.generation(1), None);
        assert_eq!(runtime().root_tlabs.generation(usize::MAX), None);
        let old = willow_alloc_typed(8, 0);
        assert_eq!(runtime().root_tlabs.generation(old as usize), None);
        {
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_unindexed(&mut state);
        }
        assert_eq!(
            runtime().root_tlabs.generation(young as usize),
            Some(GC_GENERATION_YOUNG)
        );
        let mut slot = young;
        willow_push_root(&mut slot);
        willow_gc_minor_collect();
        assert_ne!(slot, young, "minor collection copies the live root");
        assert_eq!(
            runtime().root_tlabs.generation(slot as usize),
            Some(GC_GENERATION_YOUNG)
        );
        willow_pop_roots(1);
        willow_gc_collect();
        assert_eq!(runtime().root_tlabs.generation(slot as usize), None);
        reset_internal_for_test();
        assert!(runtime().root_tlabs.0.read().unwrap().is_empty());
    }
}
