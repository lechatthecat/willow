//! Synthetic-address fixture backend. Real regions delegate to production code.
use super::*;

pub(super) enum EpochData {
    Regions(RegionIndex),
    Synthetic {
        objects: HashMap<usize, (raw_heap::TraceMetadata, usize)>,
        marks: concurrent_bitmap::ConcurrentMarkBits,
    },
}
impl From<RegionIndex> for EpochData {
    fn from(index: RegionIndex) -> Self {
        Self::Regions(index)
    }
}
impl EpochData {
    pub(super) fn from_regions(index: RegionIndex) -> Self {
        Self::Regions(index)
    }
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
    pub(super) fn snapshot_late(&self, state: &GcState, stopped_chunks: usize) -> LatePreparation {
        match self {
            Self::Regions(index) => index.snapshot_late(state, stopped_chunks),
            Self::Synthetic { .. } => unreachable!("initial capture uses regions"),
        }
    }
    pub(super) fn publish_late(&mut self, preparation: LatePreparation) {
        match self {
            Self::Regions(index) => index.publish_late(preparation),
            Self::Synthetic { .. } => unreachable!("initial capture uses regions"),
        }
    }
    pub(super) fn refresh_stopped(&mut self, state: &GcState) {
        match self {
            Self::Regions(index) => index.refresh_stopped(state),
            Self::Synthetic { .. } => *self = RegionIndex::capture_preserving_marks(state).into(),
        }
    }
    pub(super) unsafe fn claimed_metadata(&self, address: usize) -> raw_heap::TraceMetadata {
        match self {
            Self::Regions(index) => unsafe { index.claimed_metadata(address) },
            Self::Synthetic { objects, .. } => {
                objects.get(&address).expect("claimed test object exists").0
            }
        }
    }
    pub(super) unsafe fn claimed_metadata_unlocked(
        index: &std::sync::RwLock<Self>,
        address: usize,
    ) -> raw_heap::TraceMetadata {
        index
            .read()
            .unwrap()
            .metadata(address)
            .expect("claimed test object exists")
    }
    pub(super) fn uniform_extent(&self, base: usize, capacity: usize) -> Option<bool> {
        match self {
            Self::Regions(index) => index.uniform_extent(base, capacity),
            Self::Synthetic { .. } => None,
        }
    }
    pub(super) fn retains_allocated_cached(
        &self,
        address: usize,
        hint: &mut Option<usize>,
    ) -> bool {
        match self {
            Self::Regions(index) => index.retains_allocated_cached(address, hint),
            Self::Synthetic { objects, marks } => objects
                .get(&address)
                .is_none_or(|(_, bit)| marks.contains(*bit)),
        }
    }
    pub(super) fn needs_start_publication(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index.needs_start_publication(address),
            Self::Synthetic { .. } => true,
        }
    }
    pub(super) fn claim_metadata(
        &self,
        address: usize,
        hint: &mut Option<usize>,
    ) -> Option<Option<raw_heap::TraceMetadata>> {
        match self {
            Self::Regions(index) => index.claim_metadata(address, hint),
            Self::Synthetic { objects, marks } => {
                let (metadata, bit) = objects.get(&address)?;
                Some(marks.claim(*bit).then_some(*metadata))
            }
        }
    }
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Regions(index) => index.len(),
            Self::Synthetic { objects, .. } => objects.len(),
        }
    }
    pub(super) fn contains_extent(&self, base: usize) -> bool {
        match self {
            Self::Regions(index) => index.contains_extent(base),
            Self::Synthetic { .. } => true,
        }
    }
    pub(super) fn contains(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index.contains(address),
            Self::Synthetic { objects, .. } => objects.contains_key(&address),
        }
    }
    pub(super) fn claim(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index.claim(address),
            Self::Synthetic { objects, marks } => objects
                .get(&address)
                .is_some_and(|(_, bit)| marks.claim(*bit)),
        }
    }
    pub(super) fn is_marked(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index.is_marked(address),
            Self::Synthetic { objects, marks } => objects
                .get(&address)
                .is_some_and(|(_, bit)| marks.contains(*bit)),
        }
    }
    pub(super) fn retains_allocated(&self, address: usize) -> bool {
        match self {
            Self::Regions(index) => index.retains_allocated(address),
            Self::Synthetic { objects, marks } => objects
                .get(&address)
                .is_none_or(|(_, bit)| marks.contains(*bit)),
        }
    }
    pub(super) fn metadata(&self, address: usize) -> Option<raw_heap::TraceMetadata> {
        match self {
            Self::Regions(index) => index.metadata(address),
            Self::Synthetic { objects, .. } => objects.get(&address).map(|(metadata, _)| *metadata),
        }
    }
    pub(super) fn marked_count(&self) -> usize {
        match self {
            Self::Regions(index) => index
                .regions
                .iter()
                .flatten()
                .map(|v| v.marks.count())
                .sum(),
            Self::Synthetic { marks, .. } => marks.count(),
        }
    }
}
