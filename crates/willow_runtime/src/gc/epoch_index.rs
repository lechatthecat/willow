//! Region-sized epoch views. Persistent atomic start/mark bits stay with their
//! allocation; an epoch copies no per-object metadata. Collection serialization
//! and the stopped minor interlock keep backing storage alive while marker
//! readers are active. Minor relocation refreshes views only after quiescence.
use super::*;

const PAGE_BYTES: usize = 4096;
#[cfg(test)]
thread_local! {
    static READER_ACQUIRED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static REGION_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static EPOCH_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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

/// Collector serialization owns extent lifetimes. Only Arc metadata and
/// immutable extent coordinates are copied under the heap mutex; page-index
/// construction and bitmap reset run after releasing it, with phase inactive.
pub(super) struct Preparation {
    views: Vec<(RegionView, bool)>,
    old_regions: usize,
    old_objects: usize,
    objects: usize,
}
impl Preparation {
    pub(super) fn build(self, reset: bool) -> (RegionIndex, usize) {
        let mut index = RegionIndex {
            regions: Vec::with_capacity(self.views.len()),
            tlab_ids: Vec::new(),
            free_views: Vec::new(),
            last_id: 0,
            pages: IdMap::default(),
            large: IdMap::default(),
            objects: self.objects,
            old_regions: self.old_regions,
            old_objects: self.old_objects,
            #[cfg(test)]
            refresh_page_visits: 0,
        };
        let mut words = 0;
        for (position, (view, large)) in self.views.into_iter().enumerate() {
            if reset {
                words += view.marks.word_count();
                view.marks.clear();
            }
            index.add(view.base, view.capacity, &view.starts, &view.marks, large);
            if position >= self.old_regions {
                index.tlab_ids.push(index.last_view_id());
            }
        }
        (index, words)
    }
}

pub(super) struct LatePreparation {
    views: Vec<(RegionView, bool)>,
    new_old_regions: usize,
    old_regions: usize,
    old_objects: usize,
    objects: usize,
}

pub(super) struct RegionIndex {
    regions: Vec<Option<RegionView>>,
    tlab_ids: Vec<usize>,
    free_views: Vec<usize>,
    last_id: usize,
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
        // Reset happened before the stop while the collector held epoch
        // serialization. This stopped publication only captures region views.
        Self::capture_preserving_marks(state)
    }
    pub(super) fn snapshot(state: &GcState) -> Preparation {
        let mut views = Vec::with_capacity(state.old_regions.len() + state.tlab_chunks.len());
        let mut old_objects = 0;
        for region in &state.old_regions {
            views.push((
                RegionView {
                    base: region.start(),
                    capacity: region.capacity,
                    starts: region.mark_bitmap.bits.clone(),
                    marks: region.concurrent_marks.clone(),
                },
                region.kind == RegionKind::LargeObject,
            ));
            old_objects += region.allocations.len();
        }
        for chunk in &state.tlab_chunks {
            views.push((
                RegionView {
                    base: chunk.base as usize,
                    capacity: chunk.capacity,
                    starts: chunk.mark_bitmap.bits.clone(),
                    marks: chunk.concurrent_marks.clone(),
                },
                false,
            ));
        }
        Preparation {
            views,
            old_regions: state.old_regions.len(),
            old_objects,
            objects: state.total_allocs.saturating_sub(state.total_frees) as usize,
        }
    }

    fn capture_preserving_marks(state: &GcState) -> Self {
        Self::snapshot(state).build(false).0
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
        for id in self.tlab_ids.drain(..) {
            let view = self.regions[id].take().unwrap();
            for page in view.base / PAGE_BYTES..=(view.base + view.capacity - 1) / PAGE_BYTES {
                #[cfg(test)]
                {
                    self.refresh_page_visits += 1;
                }
                let candidates = self.pages.get_mut(&page).unwrap();
                for candidate in candidates.iter_mut() {
                    if *candidate == id {
                        *candidate = usize::MAX;
                    }
                }
                if candidates[0] == usize::MAX {
                    candidates.swap(0, 1);
                }
                if candidates[0] == usize::MAX {
                    self.pages.remove(&page);
                }
            }
            self.free_views.push(id);
        }
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
        self.old_regions = state.old_regions.len();
        self.old_objects = self.objects;
        for chunk in &state.tlab_chunks {
            self.add(
                chunk.base as usize,
                chunk.capacity,
                &chunk.mark_bitmap.bits,
                &chunk.concurrent_marks,
                false,
            );
            self.tlab_ids.push(self.last_view_id());
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
        #[cfg(test)]
        old_cycle::REGION_VIEWS.fetch_add(1, Ordering::Relaxed);
        let view = Some(RegionView {
            base,
            capacity,
            starts: starts.clone(),
            marks: marks.clone(),
        });
        let id = if let Some(id) = self.free_views.pop() {
            self.regions[id] = view;
            id
        } else {
            self.regions.push(view);
            self.regions.len() - 1
        };
        self.last_id = id;
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
    pub(super) fn publication_counts(&self) -> (usize, usize) {
        (self.regions.len(), self.pages.len() + self.large.len())
    }

    fn last_view_id(&self) -> usize {
        self.last_id
    }

    /// Snapshot only new extents under the heap mutex. Publish their page map
    /// after releasing that mutex and the initial global stop.
    fn snapshot_late(&self, state: &GcState) -> LatePreparation {
        let new_old_regions = state.old_regions.len() - self.old_regions;
        let mut views =
            Vec::with_capacity(new_old_regions + state.tlab_chunks.len() - self.tlab_ids.len());
        let mut old_objects = self.old_objects;
        for region in &state.old_regions[self.old_regions..] {
            views.push((
                RegionView {
                    base: region.start(),
                    capacity: region.capacity,
                    starts: region.mark_bitmap.bits.clone(),
                    marks: region.concurrent_marks.clone(),
                },
                region.kind == RegionKind::LargeObject,
            ));
            old_objects += region.allocations.len();
        }
        for chunk in &state.tlab_chunks[self.tlab_ids.len()..] {
            views.push((
                RegionView {
                    base: chunk.base as usize,
                    capacity: chunk.capacity,
                    starts: chunk.mark_bitmap.bits.clone(),
                    marks: chunk.concurrent_marks.clone(),
                },
                false,
            ));
        }
        LatePreparation {
            views,
            new_old_regions,
            old_regions: state.old_regions.len(),
            old_objects,
            objects: state.total_allocs.saturating_sub(state.total_frees) as usize,
        }
    }

    fn publish_late(&mut self, preparation: LatePreparation) {
        for (position, (view, large)) in preparation.views.into_iter().enumerate() {
            self.add(view.base, view.capacity, &view.starts, &view.marks, large);
            if position >= preparation.new_old_regions {
                self.tlab_ids.push(self.last_view_id());
            }
        }
        self.old_regions = preparation.old_regions;
        self.old_objects = preparation.old_objects;
        self.objects = preparation.objects;
    }

    #[cfg(test)]
    fn complete_initial(&mut self, state: &GcState) {
        self.publish_late(self.snapshot_late(state));
    }

    fn locate_region_id(&self, address: usize) -> Option<(usize, usize)> {
        #[cfg(test)]
        REGION_LOOKUPS.set(REGION_LOOKUPS.get() + 1);
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        if let Some(&id) = self.large.get(&header) {
            let view = self.regions[id].as_ref().unwrap();
            return view.position(header).map(|bit| (id, bit));
        }
        for &id in self.pages.get(&(header / PAGE_BYTES))? {
            if id == usize::MAX {
                break;
            }
            let view = self.regions[id].as_ref().unwrap();
            if let Some(bit) = view.position(header) {
                return Some((id, bit));
            }
        }
        None
    }
    fn locate_region(&self, address: usize) -> Option<(&RegionView, usize)> {
        let (id, bit) = self.locate_region_id(address)?;
        Some((self.regions[id].as_ref().unwrap(), bit))
    }
    fn locate_extent_cached(
        &self,
        address: usize,
        hint: &mut Option<usize>,
    ) -> Option<(&RegionView, usize)> {
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        if let Some(id) = *hint
            && let Some(view) = self.regions.get(id).and_then(Option::as_ref)
            && let Some(bit) = view.position(header)
        {
            return Some((view, bit));
        }
        let (id, bit) = self.locate_region_id(address)?;
        *hint = Some(id);
        let view = self.regions[id].as_ref().unwrap();
        Some((view, bit))
    }
    fn locate_cached(
        &self,
        address: usize,
        hint: &mut Option<usize>,
    ) -> Option<(&RegionView, usize)> {
        self.locate_extent_cached(address, hint)
            .filter(|(view, bit)| view.starts.contains(*bit))
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
/// A short reader for one copied batch of child references. Never retained
/// across a native trace callback or a safepoint.
pub(super) struct EpochReader<'a>(std::sync::RwLockReadGuard<'a, EpochData>);
impl EpochReader<'_> {
    /// Read a queue-owned header through this guard, including debug/test
    /// validation. Reacquiring the index here could deadlock behind a writer.
    ///
    /// # Safety
    /// Same claimed-allocation and active-drain requirements as
    /// `EpochIndex::claimed_metadata`.
    pub(super) unsafe fn claimed_metadata(&self, address: usize) -> raw_heap::TraceMetadata {
        match &*self.0 {
            EpochData::Regions(index) => {
                debug_assert!(index.locate(address).is_some());
                HeapObject::from_raw((address - GC_HEADER_SIZE) as *mut GcHeader)
                    .expect("claimed allocation is nonnull")
                    .trace_metadata()
            }
            #[cfg(test)]
            EpochData::Synthetic { objects, .. } => {
                objects.get(&address).expect("claimed test object exists").0
            }
        }
    }

    /// Allocations were validated by the sweep index; starts may be rebuilding.
    pub(super) fn retains_allocated_cached(
        &self,
        address: usize,
        hint: &mut Option<usize>,
    ) -> bool {
        match &*self.0 {
            EpochData::Regions(index) => index
                .locate_extent_cached(address, hint)
                .is_none_or(|(view, bit)| view.marks.contains(bit)),
            #[cfg(test)]
            EpochData::Synthetic { objects, marks } => objects
                .get(&address)
                .is_none_or(|(_, bit)| marks.contains(*bit)),
        }
    }

    pub(super) fn needs_start_publication(&self, address: usize) -> bool {
        match &*self.0 {
            EpochData::Regions(index) => index.locate_region(address).is_some(),
            #[cfg(test)]
            EpochData::Synthetic { .. } => true,
        }
    }

    /// Claim and read immutable layout through one extent lookup/read lock.
    /// `None` is unindexed; `Some(None)` is already claimed.
    pub(super) fn claim_metadata(
        &self,
        address: usize,
        hint: &mut Option<usize>,
    ) -> Option<Option<raw_heap::TraceMetadata>> {
        match &*self.0 {
            EpochData::Regions(index) => {
                let (view, bit) = index.locate_cached(address, hint)?;
                if !view.marks.claim(bit) {
                    return Some(None);
                }
                let object = HeapObject::from_raw((address - GC_HEADER_SIZE) as *mut GcHeader)
                    .expect("published start names a header");
                Some(Some(object.trace_metadata()))
            }
            #[cfg(test)]
            EpochData::Synthetic { objects, marks } => {
                let (metadata, bit) = objects.get(&address)?;
                Some(marks.claim(*bit).then_some(*metadata))
            }
        }
    }
}
impl EpochIndex {
    pub(super) fn regions(index: RegionIndex) -> Self {
        Self(std::sync::RwLock::new(EpochData::Regions(index)))
    }
    pub(super) fn snapshot_late(&self, state: &GcState) -> LatePreparation {
        match &*self.0.read().unwrap() {
            EpochData::Regions(index) => index.snapshot_late(state),
            #[cfg(test)]
            EpochData::Synthetic { .. } => unreachable!("initial capture uses regions"),
        }
    }
    pub(super) fn publish_late(&self, preparation: LatePreparation) {
        match &mut *self.0.write().unwrap() {
            EpochData::Regions(index) => index.publish_late(preparation),
            #[cfg(test)]
            EpochData::Synthetic { .. } => unreachable!("initial capture uses regions"),
        }
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
    pub(super) fn reader(&self) -> EpochReader<'_> {
        #[cfg(test)]
        EPOCH_READS.set(EPOCH_READS.get() + 1);
        let reader = EpochReader(self.0.read().unwrap());
        #[cfg(test)]
        if let Some(hook) = READER_ACQUIRED.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        reader
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
    /// Read a queue-owned object's immutable header without repeating region
    /// lookup. The active drain excludes relocation and reclamation.
    ///
    /// # Safety
    /// `address` must be a validated allocation published to this epoch's
    /// queue (or a child successfully claimed through its start bitmap).
    pub(super) unsafe fn claimed_metadata(&self, address: usize) -> raw_heap::TraceMetadata {
        #[cfg(test)]
        {
            self.metadata(address).expect("claimed test object exists")
        }
        #[cfg(not(test))]
        {
            debug_assert!(self.contains(address));
            HeapObject::from_raw((address - GC_HEADER_SIZE) as *mut GcHeader)
                .expect("claimed allocation is nonnull")
                .trace_metadata()
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
            EpochData::Regions(index) => index
                .regions
                .iter()
                .flatten()
                .map(|v| v.marks.count())
                .sum(),
            EpochData::Synthetic { marks, .. } => marks.count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_metadata_with_waiting_late_publisher_completes() {
        use std::time::{Duration, Instant};
        const KEY: &str = "WILLOW_EPOCH_READER_RACE_CHILD";
        const TEST: &str =
            "gc::epoch_index::tests::claimed_metadata_with_waiting_late_publisher_completes";
        let Ok(mode) = std::env::var(KEY) else {
            // A regressed recursive read cannot be unwound. Bound each shape
            // in its own child so failure never wedges the test runner.
            for mode in ["object", "batch"] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(KEY, mode)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        let output = child.wait_with_output().unwrap();
                        panic!("{mode} drain/publisher deadlocked: {output:?}");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{mode}: {output:?}");
            }
            return;
        };
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let objects = [willow_alloc_typed(8, 1), willow_alloc_typed(8, 1)];
        let index = RegionIndex::capture(&runtime().heap.lock().unwrap());
        let cycle = Arc::new(ConcurrentCycle::with_index(
            EpochIndex::regions(index),
            IdSet::default(),
            IdMap::default(),
            0,
        ));
        let late_object = willow_gc_alloc_layout(0, 0, GC_OLD_REGION_SIZE as i64, 0);
        let late = cycle.objects.snapshot_late(&runtime().heap.lock().unwrap());
        assert!(!cycle.objects.contains(late_object as usize));
        if mode == "object" {
            for object in objects {
                cycle.enqueue(object);
            }
        } else {
            assert_eq!(mode, "batch");
            cycle.enqueue_satb_batch(&objects.map(|object| object as usize));
        }
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let publisher_cycle = cycle.clone();
        let publisher = std::thread::spawn(move || {
            start_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            // The generated-object reader is already held: this publication
            // cannot acquire its write lock until that reader is released.
            assert!(matches!(
                publisher_cycle.objects.0.try_write(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            waiting_tx.send(()).unwrap();
            publisher_cycle.objects.publish_late(late);
        });
        let reader_cycle = cycle.clone();
        READER_ACQUIRED.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                start_tx.send(()).unwrap();
                waiting_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                // Linux's writer-preferring lock exposes the queued writer by
                // denying a fresh read. Other OS policies may allow new readers.
                let deadline = Instant::now() + Duration::from_millis(100);
                let mut writer_queued = false;
                while Instant::now() < deadline {
                    if matches!(
                        reader_cycle.objects.0.try_read(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ) {
                        writer_queued = true;
                        break;
                    }
                    std::thread::yield_now();
                }
                if cfg!(target_os = "linux") {
                    assert!(writer_queued, "publisher did not queue");
                }
            }))
        });
        let mut worker = cycle.queue.register_assist();
        let (scanned, _) = cycle.drain_worker(8, &mut Vec::new(), &mut worker);
        assert_eq!(scanned, 2);
        assert!(READER_ACQUIRED.with(|slot| slot.borrow().is_none()));
        publisher.join().unwrap();
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert!(cycle.objects.0.try_write().is_ok(), "epoch reader leaked");
        assert!(cycle.objects.contains(late_object as usize));
        for object in objects {
            assert!(cycle.is_marked(object as usize));
        }
        assert_eq!(cycle.work.lock().unwrap().scanned_bytes, 16);
        assert!(cycle.queue.snapshot().is_drained());
        drop(worker);
        assert_eq!(cycle.queue.end_epoch(), 0);
        drop(cycle);
        reset_internal_for_test();
    }

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
            let mut bound = 0;
            for round in 0..64 {
                let state = runtime().heap.lock().unwrap();
                cycle.objects.refresh_stopped(&state);
                let guard = cycle.objects.0.read().unwrap();
                let EpochData::Regions(index) = &*guard else {
                    unreachable!()
                };
                if round == 0 {
                    bound = index.regions.len();
                }
                assert_eq!(
                    index.regions.len(),
                    bound,
                    "retired view slots must be reused"
                );
                assert!(index.refresh_page_visits <= 2 * (GC_TLAB_CHUNK_SIZE / PAGE_BYTES + 1));
            }
            println!(
                "minor_refresh old_objects={count} repeated_refreshes=64 view_slot_bound={bound}"
            );
            runtime().runtime_root_slots.remove(slot);
            GC_MARK_PHASE.store(0, Ordering::Release);
            marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
            cycle.queue.end_epoch();
            willow_pop_roots(count as i32);
        }
        reset_internal_for_test();
    }

    #[test]
    fn epoch_preparation_phase_zero_allocation_race_preserves_pre_activation_edges() {
        let _test = runtime_test_guard();
        for _ in 0..32 {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let collector = cycle_wait::CollectorGuard::try_acquire().unwrap();
            let mut root = willow_alloc_typed(8, 1);
            willow_push_root(&mut root);
            // Stale previous-epoch colors must be reset, while allocation only
            // publishes starts and immutable headers under phase zero.
            let snapshot = {
                let state = runtime().heap.lock().unwrap();
                state.old_regions[0].concurrent_marks.set(0);
                RegionIndex::snapshot(&state)
            };
            let begin = Arc::new(std::sync::Barrier::new(2));
            let publisher = {
                let begin = begin.clone();
                let address = root as usize;
                std::thread::spawn(move || {
                    let _mutator = MutatorRegistration::new();
                    begin.wait();
                    let child = willow_gc_alloc_layout(0, 0, GC_OLD_REGION_SIZE as i64, 0);
                    unsafe {
                        store_gc_reference(address as *mut *mut u8, child);
                    }
                    child as usize
                })
            };
            begin.wait();
            let (prepared, _) = snapshot.build(true);
            let child = publisher.join().unwrap();
            let cycle = Arc::new(ConcurrentCycle::with_index(
                EpochIndex::regions(prepared),
                IdSet::default(),
                IdMap::default(),
                1,
            ));
            let late = {
                let mut state = runtime().heap.lock().unwrap();
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                cycle.objects.snapshot_late(&state)
            };
            cycle.objects.publish_late(late);
            cycle.enqueue(root);
            let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
            assert!(cycle.is_marked(root as usize));
            assert!(cycle.is_marked(child));
            sweep::concurrent(plan);
            assert_eq!(
                willow_gc_allocated_bytes(),
                (2 * GC_HEADER_SIZE + 8 + GC_OLD_REGION_SIZE) as i64
            );
            willow_pop_roots(1);
            drop(collector);
        }
        reset_internal_for_test();
    }

    #[test]
    fn initial_completion_publishes_late_extents_without_rebuilding_prepared_pages() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let first = willow_alloc(8);
        let mut index = RegionIndex::capture(&runtime().heap.lock().unwrap());
        let old_pages = index.pages.clone();
        let before = index.refresh_page_visits;
        let mut tls = tlab_state_for_test();
        let young = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        let late = willow_gc_alloc_layout(0, 0, GC_OLD_REGION_SIZE as i64, 0);
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        index.complete_initial(&state);
        drop(state);
        assert_prepared_candidates_preserved(&index, &old_pages);
        // Dedicated late objects add no payload pages; only the new TLAB does.
        assert!(
            index.refresh_page_visits - before <= GC_TLAB_CHUNK_SIZE / PAGE_BYTES + 1,
            "late publication rebuilt prepared pages: before={before}, after={}",
            index.refresh_page_visits
        );
        let epoch = EpochIndex::regions(index);
        for pointer in [first, young, late] {
            assert!(
                epoch.claim(pointer as usize),
                "missing or already marked object {pointer:p}"
            );
        }
        reset_internal_for_test();
    }

    fn assert_prepared_candidates_preserved(index: &RegionIndex, old: &IdMap<usize, [usize; 2]>) {
        for (&page, candidates) in old {
            for (slot, &id) in candidates.iter().enumerate() {
                // A late, disjoint extent may occupy the unused slot on a
                // shared boundary page. Only existing candidates are immutable.
                if id != usize::MAX {
                    assert_eq!(
                        index.pages[&page][slot], id,
                        "prepared page {page}, slot {slot}"
                    );
                }
            }
        }
    }

    #[test]
    fn late_publication_preserves_candidates_on_shared_boundary_pages() {
        for count in [1, 16, 64, 256] {
            let mut index = empty();
            let view = |base| {
                let starts = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                    PAGE_BYTES / GC_REGION_MARK_GRANULE,
                ));
                starts.set(0);
                RegionView {
                    base,
                    capacity: PAGE_BYTES,
                    starts,
                    marks: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                        PAGE_BYTES / GC_REGION_MARK_GRANULE,
                    )),
                }
            };
            for id in 0..count {
                let v = view(GC_REGION_MARK_GRANULE + id * PAGE_BYTES);
                index.add(v.base, v.capacity, &v.starts, &v.marks, false);
            }
            let old = index.pages.clone();
            let before = index.refresh_page_visits;
            index.publish_late(LatePreparation {
                views: vec![(view(GC_REGION_MARK_GRANULE + count * PAGE_BYTES), false)],
                new_old_regions: 0,
                old_regions: count,
                old_objects: count,
                objects: count + 1,
            });
            // Reproduces the old equality assertion's failure deterministically.
            assert_ne!(index.pages[&count], old[&count]);
            assert_eq!(index.pages[&count], [count - 1, count]);
            assert_prepared_candidates_preserved(&index, &old);
            assert_eq!(index.refresh_page_visits - before, 2);
            let epoch = EpochIndex::regions(index);
            for id in 0..=count {
                let address = GC_REGION_MARK_GRANULE + id * PAGE_BYTES + GC_HEADER_SIZE;
                assert!(epoch.claim(address), "extent {id} of {count}");
                assert!(!epoch.claim(address));
            }
            println!("late publication prepared={count} added_page_visits=2");
        }
    }

    fn empty() -> RegionIndex {
        RegionIndex {
            regions: Vec::new(),
            tlab_ids: Vec::new(),
            free_views: Vec::new(),
            last_id: 0,
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
                assert!(Arc::ptr_eq(
                    &index.regions[id].as_ref().unwrap().marks,
                    &marks
                ));
                assert!(Arc::ptr_eq(
                    &index.regions[id].as_ref().unwrap().starts,
                    &starts
                ));
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
    #[test]
    fn cached_epoch_region_bounds_and_query_counts() {
        for count in [1usize, 10, 100] {
            let mut index = empty();
            for i in 0..count {
                let starts = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                    PAGE_BYTES / GC_REGION_MARK_GRANULE,
                ));
                let marks = Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                    PAGE_BYTES / GC_REGION_MARK_GRANULE,
                ));
                starts.set(0);
                starts.set(2);
                index.add((i + 1) * 2 * PAGE_BYTES, PAGE_BYTES, &starts, &marks, false);
            }
            let mut hint = None;
            REGION_LOOKUPS.set(0);
            for i in 0..count {
                let base = (i + 1) * 2 * PAGE_BYTES;
                for _ in 0..100 {
                    for bit in [0, 2] {
                        let address = base + bit * GC_REGION_MARK_GRANULE + GC_HEADER_SIZE;
                        let (view, found) = index.locate_cached(address, &mut hint).unwrap();
                        assert_eq!((view.base, found), (base, bit));
                    }
                }
            }
            assert_eq!(REGION_LOOKUPS.get(), count);
            for initial in [None, Some(0), Some(usize::MAX)] {
                hint = initial;
                for address in [
                    0,
                    1,
                    2 * PAGE_BYTES + GC_HEADER_SIZE + 1,
                    3 * PAGE_BYTES + GC_HEADER_SIZE,
                ] {
                    assert!(index.locate_cached(address, &mut hint).is_none());
                }
            }
            for view in index.regions.iter().flatten() {
                view.starts.clear();
                view.marks.set(2);
            }
            let epoch = EpochIndex::regions(index);
            let reader = epoch.reader();
            hint = None;
            REGION_LOOKUPS.set(0);
            for i in 0..count {
                let base = (i + 1) * 2 * PAGE_BYTES;
                for _ in 0..100 {
                    for bit in [0, 2] {
                        let address = base + bit * GC_REGION_MARK_GRANULE + GC_HEADER_SIZE;
                        assert_eq!(
                            reader.retains_allocated_cached(address, &mut hint),
                            bit == 2
                        );
                    }
                }
            }
            assert_eq!(REGION_LOOKUPS.get(), count);
            // New extents are implicitly black, even after a cached old extent.
            assert!(reader.retains_allocated_cached(
                (count + 2) * 2 * PAGE_BYTES + GC_HEADER_SIZE,
                &mut hint,
            ));
            eprintln!("epoch references={} region_queries={count}", count * 200);
        }
    }
}
