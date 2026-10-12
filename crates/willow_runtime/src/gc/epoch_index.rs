//! Region-sized epoch views. Persistent atomic start/mark bits stay with their
//! allocation; an epoch copies no per-object metadata. Collection serialization
//! and the stopped minor interlock keep backing storage alive while marker
//! readers are active. Minor relocation refreshes views only after quiescence.
use super::*;

const PAGE_BYTES: usize = 4096;

struct RegionView {
    base: usize,
    capacity: usize,
    starts: Arc<concurrent_bitmap::ConcurrentMarkBits>,
    marks: Arc<concurrent_bitmap::ConcurrentMarkBits>,
}
impl RegionView {
    fn position(&self, header: usize) -> Option<usize> {
        let offset = header.checked_sub(self.base)?;
        (offset < self.capacity && offset.is_multiple_of(GC_REGION_MARK_GRANULE))
            .then_some(offset / GC_REGION_MARK_GRANULE)
    }
}

pub(super) struct RegionIndex {
    regions: Vec<RegionView>,
    // Fixed-size regular regions/chunks intersect a bounded number of pages.
    // Disjoint allocations of at least one page yield at most two candidates
    // per page, even when the underlying allocator does not page-align them.
    pages: IdMap<usize, [usize; 2]>,
    // Dedicated large allocations have exactly one start. Never index their
    // payload pages or allocate mark metadata proportional to payload size.
    large: IdMap<usize, usize>,
    objects: usize,
    old_regions: usize,
    old_objects: usize,
    #[cfg(test)]
    refresh_page_visits: usize,
}
impl RegionIndex {
    pub(super) fn capture(state: &GcState) -> Self {
        let index = Self::capture_preserving_marks(state);
        for region in &index.regions {
            region.marks.clear();
        }
        index
    }
    fn capture_preserving_marks(state: &GcState) -> Self {
        let mut index = Self {
            regions: Vec::with_capacity(state.old_regions.len() + state.tlab_chunks.len()),
            pages: IdMap::default(),
            large: IdMap::default(),
            objects: 0,
            old_regions: 0,
            old_objects: 0,
            #[cfg(test)]
            refresh_page_visits: 0,
        };
        for region in &state.old_regions {
            index.add(
                region.start(),
                region.capacity,
                &region.mark_bitmap.bits,
                &region.concurrent_marks,
                region.kind == RegionKind::LargeObject,
            );
            index.objects += region.allocations.len();
        }
        index.old_regions = index.regions.len();
        index.old_objects = index.objects;
        for chunk in &state.tlab_chunks {
            index.add(
                chunk.base as usize,
                chunk.capacity,
                &chunk.mark_bitmap.bits,
                &chunk.concurrent_marks,
                false,
            );
            // A chunk retired since the last walk has no header index yet; its
            // start bits count its headers in O(chunk words).
            index.objects += if chunk.needs_index() {
                chunk.mark_bitmap.bits.count()
            } else {
                chunk.header_offsets.len()
            };
        }
        index
    }
    /// Old extents cannot disappear during minor STW. Retain their page index
    /// and rebuild only TLAB extents plus newly allocated old regions. Work is
    /// independent of existing old-region pages/payload bytes. Retained pinned
    /// TLABs remain part of the TLAB extent count.
    fn refresh_stopped(&mut self, state: &GcState) {
        assert!(state.old_regions.len() >= self.old_regions);
        #[cfg(test)]
        {
            self.refresh_page_visits = 0;
        }
        for (offset, view) in self.regions[self.old_regions..].iter().enumerate() {
            for page in view.base / PAGE_BYTES..=(view.base + view.capacity - 1) / PAGE_BYTES {
                #[cfg(test)]
                {
                    self.refresh_page_visits += 1;
                }
                let candidates = self.pages.get_mut(&page).unwrap();
                for id in candidates.iter_mut() {
                    if *id == self.old_regions + offset {
                        *id = usize::MAX;
                    }
                }
                if candidates[0] == usize::MAX {
                    candidates.swap(0, 1);
                }
                if candidates[0] == usize::MAX {
                    self.pages.remove(&page);
                }
            }
        }
        self.regions.truncate(self.old_regions);
        self.objects = self.old_objects;
        for region in &state.old_regions[self.old_regions..] {
            self.add(
                region.start(),
                region.capacity,
                &region.mark_bitmap.bits,
                &region.concurrent_marks,
                region.kind == RegionKind::LargeObject,
            );
            self.objects += region.allocations.len();
        }
        self.old_regions = self.regions.len();
        self.old_objects = self.objects;
        for chunk in &state.tlab_chunks {
            self.add(
                chunk.base as usize,
                chunk.capacity,
                &chunk.mark_bitmap.bits,
                &chunk.concurrent_marks,
                false,
            );
            self.objects += if chunk.needs_index() {
                chunk.mark_bitmap.bits.count()
            } else {
                chunk.header_offsets.len()
            };
        }
    }

    fn add(
        &mut self,
        base: usize,
        capacity: usize,
        starts: &Arc<concurrent_bitmap::ConcurrentMarkBits>,
        marks: &Arc<concurrent_bitmap::ConcurrentMarkBits>,
        large: bool,
    ) {
        let id = self.regions.len();
        self.regions.push(RegionView {
            base,
            capacity,
            starts: starts.clone(),
            marks: marks.clone(),
        });
        if large {
            self.large.insert(base, id);
        } else {
            assert!(
                capacity >= PAGE_BYTES,
                "regular GC regions must span at least one index page"
            );
            for page in base / PAGE_BYTES..=(base + capacity - 1) / PAGE_BYTES {
                #[cfg(test)]
                {
                    self.refresh_page_visits += 1;
                }
                let candidates = self.pages.entry(page).or_insert([usize::MAX; 2]);
                let slot = candidates
                    .iter_mut()
                    .find(|slot| **slot == usize::MAX)
                    .expect("disjoint page-sized regions have at most two candidates per page");
                *slot = id;
            }
        }
    }
    fn locate_region(&self, address: usize) -> Option<(&RegionView, usize)> {
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        if let Some(&id) = self.large.get(&header) {
            let view = &self.regions[id];
            return view.position(header).map(|bit| (view, bit));
        }
        for &id in self.pages.get(&(header / PAGE_BYTES))? {
            if id == usize::MAX {
                break;
            }
            let view = &self.regions[id];
            if let Some(bit) = view.position(header) {
                return Some((view, bit));
            }
        }
        None
    }
    fn locate(&self, address: usize) -> Option<(&RegionView, usize)> {
        self.locate_region(address)
            .filter(|(view, bit)| view.starts.contains(*bit))
    }
}

enum EpochData {
    Regions(RegionIndex),
    // Queue/worker unit tests also use synthetic addresses with no raw headers.
    #[cfg(test)]
    Synthetic {
        objects: HashMap<usize, (raw_heap::TraceMetadata, usize)>,
        marks: concurrent_bitmap::ConcurrentMarkBits,
    },
}
pub(super) struct EpochIndex(std::sync::RwLock<EpochData>);
impl EpochIndex {
    pub(super) fn regions(index: RegionIndex) -> Self {
        Self(std::sync::RwLock::new(EpochData::Regions(index)))
    }
    pub(super) fn refresh_stopped(&self, state: &GcState) {
        let mut index = self.0.write().unwrap();
        match &mut *index {
            EpochData::Regions(index) => index.refresh_stopped(state),
            #[cfg(test)]
            EpochData::Synthetic { .. } => {
                *index = EpochData::Regions(RegionIndex::capture_preserving_marks(state));
            }
        }
    }
    #[cfg(test)]
    pub(super) fn synthetic(
        objects: impl IntoIterator<Item = (usize, raw_heap::TraceMetadata)>,
    ) -> Self {
        let objects = objects.into_iter();
        let mut map = HashMap::with_capacity(objects.size_hint().0);
        for (address, metadata) in objects {
            let bit = map.len();
            assert!(map.insert(address, (metadata, bit)).is_none());
        }
        let marks = concurrent_bitmap::ConcurrentMarkBits::new(map.len());
        Self(std::sync::RwLock::new(EpochData::Synthetic {
            objects: map,
            marks,
        }))
    }
    pub(super) fn len(&self) -> usize {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index.objects,
            #[cfg(test)]
            EpochData::Synthetic { objects, .. } => objects.len(),
        }
    }
    /// Extent membership does not depend on a published object-start bit.
    /// TLABs created after the last stopped capture are implicitly black until
    /// the next minor publishes their surviving starts into this index.
    pub(super) fn contains_extent(&self, base: usize) -> bool {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index.locate_region(base + GC_HEADER_SIZE).is_some(),
            #[cfg(test)]
            EpochData::Synthetic { .. } => true,
        }
    }
    pub(super) fn contains(&self, address: usize) -> bool {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index.locate(address).is_some(),
            #[cfg(test)]
            EpochData::Synthetic { objects, .. } => objects.contains_key(&address),
        }
    }
    pub(super) fn claim(&self, address: usize) -> bool {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index
                .locate(address)
                .is_some_and(|(view, bit)| view.marks.claim(bit)),
            #[cfg(test)]
            EpochData::Synthetic { objects, marks } => objects
                .get(&address)
                .is_some_and(|(_, bit)| marks.claim(*bit)),
        }
    }
    pub(super) fn is_marked(&self, address: usize) -> bool {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index
                .locate(address)
                .is_some_and(|(view, bit)| view.marks.contains(bit)),
            #[cfg(test)]
            EpochData::Synthetic { objects, marks } => objects
                .get(&address)
                .is_some_and(|(_, bit)| marks.contains(*bit)),
        }
    }
    /// Sweep already validated the allocation. The start map may be getting
    /// rebuilt, so only consult the stable epoch extent and reachability bits.
    /// Extents allocated after capture survive their first cycle.
    pub(super) fn retains_allocated(&self, address: usize) -> bool {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index
                .locate_region(address)
                .is_none_or(|(view, bit)| view.marks.contains(bit)),
            #[cfg(test)]
            EpochData::Synthetic { objects, marks } => objects
                .get(&address)
                .is_none_or(|(_, bit)| marks.contains(*bit)),
        }
    }
    pub(super) fn metadata(&self, address: usize) -> Option<raw_heap::TraceMetadata> {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => {
                index.locate(address)?;
                // Start publication follows header initialization. Only immutable
                // layout fields are read; sweep/copy wait for epoch readers.
                HeapObject::from_raw((address - GC_HEADER_SIZE) as *mut GcHeader)
                    .map(|o| o.trace_metadata())
            }
            #[cfg(test)]
            EpochData::Synthetic { objects, .. } => {
                objects.get(&address).map(|(metadata, _)| *metadata)
            }
        }
    }
    #[cfg(test)]
    pub(super) fn marked_count(&self) -> usize {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index.regions.iter().map(|v| v.marks.count()).sum(),
            EpochData::Synthetic { marks, .. } => marks.count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minor_refresh_does_not_rebuild_existing_old_pages_or_clear_colors() {
        let _guard = runtime_test_guard();
        for count in [1, 16, 64, 256] {
            reset_internal_for_test();
            let mut roots = Vec::with_capacity(count);
            // Separate regular old regions make an adverse old-heap dimension.
            for _ in 0..count {
                roots.push(willow_gc_alloc_layout(
                    1,
                    0,
                    (GC_OLD_REGION_SIZE / 2) as i64,
                    0,
                ));
                willow_push_root(roots.last_mut().unwrap());
            }
            let mut tls = tlab_state_for_test();
            let mut young = willow_gc_alloc_slow(&mut tls, 2, 0, 8, 0);
            let slot = (&mut young as *mut *mut u8).cast::<u8>();
            runtime().runtime_root_slots.add(slot);
            let (_, cycle) = root_handshake::begin().unwrap();
            for &root in &roots {
                cycle.objects.claim(root as usize);
            }
            minor::collect_owned();
            let guard = cycle.objects.0.read().unwrap();
            let EpochData::Regions(index) = &*guard else {
                unreachable!()
            };
            // One source and one destination TLAB. Existing old pages cost zero.
            let bound = 2 * (GC_TLAB_CHUNK_SIZE / PAGE_BYTES + 1);
            assert!(index.refresh_page_visits <= bound);
            eprintln!(
                "minor_refresh old_objects={count} page_visits={} bound={bound}",
                index.refresh_page_visits
            );
            drop(guard);
            for &root in &roots {
                assert!(cycle.is_marked(root as usize));
            }
            assert!(cycle.is_marked(young as usize));
            runtime().runtime_root_slots.remove(slot);
            GC_MARK_PHASE.store(0, Ordering::Release);
            marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
            cycle.queue.end_epoch();
            willow_pop_roots(count as i32);
        }
        reset_internal_for_test();
    }

    fn empty() -> RegionIndex {
        RegionIndex {
            regions: Vec::new(),
            pages: IdMap::default(),
            large: IdMap::default(),
            objects: 0,
            old_regions: 0,
            old_objects: 0,
            #[cfg(test)]
            refresh_page_visits: 0,
        }
    }

    #[test]
    fn page_index_has_bounded_candidates_and_reuses_persistent_atomic_storage() {
        for count in [1, 16, 64, 256, 1024] {
            let mut index = empty();
            let mut maps = Vec::new();
            for id in 0..count {
                // Deliberately unaligned and adjacent: shared boundary pages
                // exercise both candidates, exact ends and zero-sized payloads.
                let base = 8 + id * GC_OLD_REGION_SIZE;
                let starts = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                    GC_OLD_REGION_SIZE / GC_REGION_MARK_GRANULE,
                ));
                let marks = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                    GC_OLD_REGION_SIZE / GC_REGION_MARK_GRANULE,
                ));
                for offset in [0, GC_OLD_REGION_SIZE - GC_HEADER_SIZE] {
                    starts.set(offset / GC_REGION_MARK_GRANULE);
                    marks.set(offset / GC_REGION_MARK_GRANULE);
                }
                marks.clear();
                index.add(base, GC_OLD_REGION_SIZE, &starts, &marks, false);
                assert!(Arc::ptr_eq(&index.regions[id].marks, &marks));
                assert!(Arc::ptr_eq(&index.regions[id].starts, &starts));
                assert_eq!(marks.count(), 0, "new epoch resets persistent color");
                maps.push((base, starts, marks));
            }
            assert_eq!(
                index.pages.len(),
                count * (GC_OLD_REGION_SIZE / PAGE_BYTES) + 1
            );
            let epoch = Arc::new(EpochIndex::regions(index));
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let epoch = epoch.clone();
                    std::thread::spawn(move || {
                        (0..count)
                            .flat_map(|id| {
                                [
                                    8 + id * GC_OLD_REGION_SIZE + GC_HEADER_SIZE,
                                    8 + (id + 1) * GC_OLD_REGION_SIZE,
                                ]
                            })
                            .filter(|&address| epoch.claim(address))
                            .count()
                    })
                })
                .collect();
            assert_eq!(
                workers
                    .into_iter()
                    .map(|w| w.join().unwrap())
                    .sum::<usize>(),
                2 * count
            );
            for (base, starts, marks) in maps {
                assert!(!epoch.contains(base));
                assert!(!epoch.contains(base + GC_HEADER_SIZE + 1));
                assert!(!epoch.contains(base + 64 + GC_HEADER_SIZE));
                // A black allocation publishes color before its object-start.
                marks.set(64 / GC_REGION_MARK_GRANULE);
                starts.set(64 / GC_REGION_MARK_GRANULE);
                assert!(epoch.contains(base + 64 + GC_HEADER_SIZE));
                assert!(!epoch.claim(base + 64 + GC_HEADER_SIZE));
                // Sweep rebuilds starts, but liveness uses stable mark bits.
                starts.clear();
                assert!(epoch.retains_allocated(base + GC_HEADER_SIZE));
                assert!(!epoch.retains_allocated(base + 128 + GC_HEADER_SIZE));
            }
            println!(
                "epoch regions={count} header_copies=0 pages={} max_candidates=2 claims={}",
                count * 64 + 1,
                2 * count
            );
        }
    }

    #[test]
    fn dedicated_large_epoch_metadata_is_independent_of_payload_size() {
        for capacity in [GC_OLD_REGION_SIZE, 1 << 24, 1usize << 40] {
            let mut index = empty();
            let starts = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(1));
            let marks = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(1));
            starts.set(0);
            index.add(PAGE_BYTES, capacity, &starts, &marks, true);
            assert!(index.pages.is_empty());
            assert_eq!(index.large.len(), 1);
            let epoch = EpochIndex::regions(index);
            assert!(epoch.claim(PAGE_BYTES + GC_HEADER_SIZE));
            assert!(!epoch.claim(PAGE_BYTES + GC_HEADER_SIZE));
            assert!(!epoch.contains(PAGE_BYTES + GC_HEADER_SIZE + GC_REGION_MARK_GRANULE));
            assert_eq!(marks.count(), 1);
            println!("large_capacity={capacity} page_entries=0 exact_entries=1 mark_words=1");
        }
    }
}
