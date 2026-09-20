//! Region-sized epoch views. Persistent atomic start/mark bits stay with their
//! allocation; an epoch copies no per-object metadata. Collection serialization
//! keeps backing storage alive until all marker readers have left the epoch.
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
    pages: HashMap<usize, [usize; 2]>,
    // Dedicated large allocations have exactly one start. Never index their
    // payload pages or allocate mark metadata proportional to payload size.
    large: HashMap<usize, usize>,
    objects: usize,
}
impl RegionIndex {
    pub(super) fn capture(state: &GcState) -> Self {
        let mut index = Self {
            regions: Vec::with_capacity(state.old_regions.len() + state.tlab_chunks.len()),
            pages: HashMap::new(),
            large: HashMap::new(),
            objects: 0,
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
        for chunk in &state.tlab_chunks {
            index.add(
                chunk.base as usize,
                chunk.capacity,
                &chunk.mark_bitmap.bits,
                &chunk.concurrent_marks,
                false,
            );
            index.objects += chunk.header_offsets.len();
        }
        index
    }
    fn add(
        &mut self,
        base: usize,
        capacity: usize,
        starts: &Arc<concurrent_bitmap::ConcurrentMarkBits>,
        marks: &Arc<concurrent_bitmap::ConcurrentMarkBits>,
        large: bool,
    ) {
        marks.clear(); // Initial stop; previous epoch has no remaining readers.
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

pub(super) enum EpochIndex {
    Regions(RegionIndex),
    // Queue/worker unit tests also use synthetic addresses with no raw headers.
    #[cfg(test)]
    Synthetic {
        objects: HashMap<usize, (raw_heap::TraceMetadata, usize)>,
        marks: concurrent_bitmap::ConcurrentMarkBits,
    },
}
impl EpochIndex {
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
        Self::Synthetic {
            objects: map,
            marks,
        }
    }
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Regions(index) => index.objects,
            #[cfg(test)]
            Self::Synthetic { objects, .. } => objects.len(),
        }
    }
    pub(super) fn contains(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index.locate(address).is_some(),
            #[cfg(test)]
            Self::Synthetic { objects, .. } => objects.contains_key(&address),
        }
    }
    pub(super) fn claim(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index
                .locate(address)
                .is_some_and(|(view, bit)| view.marks.claim(bit)),
            #[cfg(test)]
            Self::Synthetic { objects, marks } => objects
                .get(&address)
                .is_some_and(|(_, bit)| marks.claim(*bit)),
        }
    }
    pub(super) fn is_marked(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index
                .locate(address)
                .is_some_and(|(view, bit)| view.marks.contains(bit)),
            #[cfg(test)]
            Self::Synthetic { objects, marks } => objects
                .get(&address)
                .is_some_and(|(_, bit)| marks.contains(*bit)),
        }
    }
    /// Sweep already validated the allocation. The start map may be getting
    /// rebuilt, so only consult the stable epoch extent and reachability bits.
    /// Extents allocated after capture survive their first cycle.
    pub(super) fn retains_allocated(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index
                .locate_region(address)
                .is_none_or(|(view, bit)| view.marks.contains(bit)),
            #[cfg(test)]
            Self::Synthetic { .. } => !self.contains(address) || self.is_marked(address),
        }
    }
    pub(super) fn metadata(&self, address: usize) -> Option<raw_heap::TraceMetadata> {
        match self {
            Self::Regions(index) => {
                index.locate(address)?;
                // Start publication follows header initialization. Only immutable
                // layout fields are read; sweep/copy wait for epoch readers.
                HeapObject::from_raw((address - GC_HEADER_SIZE) as *mut GcHeader)
                    .map(|o| o.trace_metadata())
            }
            #[cfg(test)]
            Self::Synthetic { objects, .. } => objects.get(&address).map(|(metadata, _)| *metadata),
        }
    }
    #[cfg(test)]
    pub(super) fn marked_count(&self) -> usize {
        match self {
            Self::Regions(index) => index.regions.iter().map(|v| v.marks.count()).sum(),
            Self::Synthetic { marks, .. } => marks.count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> RegionIndex {
        RegionIndex {
            regions: Vec::new(),
            pages: HashMap::new(),
            large: HashMap::new(),
            objects: 0,
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
            let epoch = Arc::new(EpochIndex::Regions(index));
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
            let epoch = EpochIndex::Regions(index);
            assert!(epoch.claim(PAGE_BYTES + GC_HEADER_SIZE));
            assert!(!epoch.claim(PAGE_BYTES + GC_HEADER_SIZE));
            assert!(!epoch.contains(PAGE_BYTES + GC_HEADER_SIZE + GC_REGION_MARK_GRANULE));
            assert_eq!(marks.count(), 1);
            println!("large_capacity={capacity} page_entries=0 exact_entries=1 mark_words=1");
        }
    }
}
