use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionKind {
    Nursery,
    Survivor,
    Old,
    LargeObject,
    Pinned,
}

pub(super) struct RegionMarkBitmap {
    pub(super) bits: Arc<concurrent_bitmap::ConcurrentMarkBits>,
}

impl RegionMarkBitmap {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            bits: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                capacity.div_ceil(GC_REGION_MARK_GRANULE),
            )),
        }
    }
    pub(super) fn clear(&mut self) {
        self.bits.clear();
    }
    pub(super) fn mark(&mut self, offset: usize) {
        self.bits.set(offset / GC_REGION_MARK_GRANULE);
    }
    pub(super) fn mark_stopped(&mut self, offset: usize) {
        self.bits.set_stopped(offset / GC_REGION_MARK_GRANULE);
    }
    pub(super) fn is_marked(&self, offset: usize) -> bool {
        self.bits.contains(offset / GC_REGION_MARK_GRANULE)
    }
    /// Highest object start in `[floor, offset]`, if any.
    pub(super) fn last_start_in(&self, floor: usize, offset: usize) -> Option<usize> {
        if floor > offset {
            return None;
        }
        self.bits
            .last_set_in(
                floor.div_ceil(GC_REGION_MARK_GRANULE),
                offset / GC_REGION_MARK_GRANULE,
            )
            .map(|index| index * GC_REGION_MARK_GRANULE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RegionFreeSpan {
    pub(super) offset: usize,
    pub(super) size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum SweepPhase {
    Unswept,
    Sweeping,
    #[default]
    Swept,
    Released,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SweepState {
    pub(super) epoch: u64,
    pub(super) phase: SweepPhase,
}

/// Metadata for one regular or large-object old-generation region.
///
/// Invariants:
/// - `[base, base + capacity)` is one allocator-owned, header-aligned block.
/// - `used <= capacity`; the unallocated bump tail is `[used, capacity)`.
/// - `allocations` maps object-header offsets to aligned physical spans.
/// - free spans are disjoint holes below `used` and never overlap allocations.
/// - `live_bytes` is the sum of logical header+payload sizes for allocations.
/// - mark bits identify object starts retained by the latest major mark/sweep.
pub(super) struct OldRegion {
    pub(super) base: *mut u8,
    pub(super) capacity: usize,
    pub(super) used: usize,
    /// Storage comes from alloc_zeroed. Only its first allocation may omit
    /// clearing; never reset this on sweep/tail trimming or span reuse.
    pristine: bool,
    pub(super) kind: RegionKind,
    pub(super) live_bytes: usize,
    pub(super) allocations: BTreeMap<usize, usize>,
    pub(super) free_spans: FreeSpans,
    pub(super) largest_free_span: usize,
    pub(super) sweep_pending: bool,
    pub(super) sweep_state: SweepState,
    pub(super) reusable_after_epoch: u64,
    pub(super) sweep_quarantined: bool,
    /// Active per-thread old allocation buffers carved from this region.
    pub(super) active_labs: usize,
    #[cfg(test)]
    pub(super) allocation_attempts: usize,
    #[cfg(test)]
    cleared_bytes: usize,
    pub(super) mark_bitmap: RegionMarkBitmap,
    pub(super) concurrent_marks: Arc<concurrent_bitmap::ConcurrentMarkBits>,
}

/// Shared indexed storage for mutator TLABs and collector-only survivor chunks.
/// Survivor chunks never have an owner_state and are never refilled by mutators.
pub(super) struct BumpChunk {
    pub(super) base: *mut u8,
    pub(super) capacity: usize,
    /// Allocated prefix in bytes. For an active chunk this is refreshed from
    /// its owner's atomic cursor when the TLAB is retired.
    pub(super) used: usize,
    pub(super) owner_state: Option<usize>,
    pub(super) kind: RegionKind,
    pub(super) live_bytes: usize,
    pub(super) mark_bitmap: RegionMarkBitmap,
    pub(super) concurrent_marks: Arc<concurrent_bitmap::ConcurrentMarkBits>,
    /// All physical headers in a retired chunk, sorted by offset. Reclaimed
    /// headers remain valid until chunk release; lookups check allocated().
    /// Built lazily by `index_retired_chunk`: a retired chunk without it was
    /// never swept or promoted from, so its start bits name exactly its
    /// published headers, all allocated and young.
    pub(super) header_offsets: Vec<u16>,
}

impl BumpChunk {
    /// A retired, nonempty chunk whose header index is not built yet.
    pub(super) fn needs_index(&self) -> bool {
        self.owner_state.is_none() && self.used != 0 && self.header_offsets.is_empty()
    }
}

pub(super) struct TlabStateRecord {
    pub(super) address: usize,
    pub(super) owner: ThreadId,
    pub(super) current_chunk: Option<usize>,
    /// Cursor just past the slow-path first object of `current_chunk`; the
    /// fast-path bytes of the active chunk are `cursor - chunk_fast_start`.
    pub(super) chunk_fast_start: usize,
    /// Fast-path totals of this TLAB's already retired chunks.
    pub(super) retired_fast_allocations: u64,
    pub(super) retired_fast_bytes: u64,
    pub(super) observed_fast_allocations: u64,
    pub(super) observed_fast_allocated_bytes: u64,
    pub(super) assist_observed_fast_bytes: u64,
}

impl RegionMarkBitmap {
    pub(super) fn unmark(&mut self, offset: usize) {
        self.bits.unset(offset / GC_REGION_MARK_GRANULE);
    }
}

impl OldRegion {
    pub(super) fn new(kind: RegionKind, capacity: usize) -> Option<Self> {
        let layout = Layout::from_size_align(capacity, std::mem::align_of::<GcHeader>()).ok()?;
        // SAFETY: `layout` is nonzero, aligned, and owned by the returned region.
        let base = unsafe { allocate_region_storage(layout) };
        if base.is_null() {
            return None;
        }
        // SAFETY: fresh zeroed storage of `capacity` bytes.
        Some(unsafe { Self::with_storage(kind, capacity, base, true) })
    }

    /// Region over released storage of exactly `capacity` bytes. Its stale
    /// contents make it non-pristine: every allocation clears its span.
    ///
    /// # Safety
    /// As for [`Self::with_storage`] with `pristine == false`.
    pub(super) unsafe fn reuse(kind: RegionKind, capacity: usize, base: *mut u8) -> Self {
        unsafe { Self::with_storage(kind, capacity, base, false) }
    }

    /// # Safety
    /// `base` came from [`allocate_region_storage`] with `capacity` bytes, is
    /// owned by no other region or cache, and is zeroed when `pristine`.
    unsafe fn with_storage(
        kind: RegionKind,
        capacity: usize,
        base: *mut u8,
        pristine: bool,
    ) -> Self {
        debug_assert!(matches!(kind, RegionKind::Old | RegionKind::LargeObject));
        let bitmap_capacity = if kind == RegionKind::LargeObject {
            GC_REGION_MARK_GRANULE
        } else {
            capacity
        };
        Self {
            base,
            capacity,
            used: 0,
            pristine,
            kind,
            live_bytes: 0,
            allocations: BTreeMap::new(),
            free_spans: FreeSpans::default(),
            largest_free_span: 0,
            sweep_pending: false,
            sweep_state: SweepState::default(),
            reusable_after_epoch: 0,
            sweep_quarantined: false,
            active_labs: 0,
            #[cfg(test)]
            allocation_attempts: 0,
            #[cfg(test)]
            cleared_bytes: 0,
            mark_bitmap: RegionMarkBitmap::new(bitmap_capacity),
            concurrent_marks: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                bitmap_capacity.div_ceil(GC_REGION_MARK_GRANULE),
            )),
        }
    }

    /// Hand an empty region's storage to the caller; dropping the region
    /// afterwards frees only its metadata.
    pub(super) fn take_storage(&mut self) -> *mut u8 {
        assert!(
            self.allocations.is_empty(),
            "released region owns no objects"
        );
        std::mem::replace(&mut self.base, std::ptr::null_mut())
    }

    pub(super) fn start(&self) -> usize {
        self.base as usize
    }

    pub(super) fn end(&self) -> usize {
        self.start().saturating_add(self.capacity)
    }

    pub(super) fn contains(&self, address: usize) -> bool {
        address >= self.start() && address < self.end()
    }

    pub(super) fn allocate_object(
        &mut self,
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        payload_size: usize,
    ) -> Option<(HeapObject, bool)> {
        self.allocate_object_for_move(type_id, layout_id, gc_ref_mask, payload_size, None)
    }

    pub(super) fn allocate_object_for_move(
        &mut self,
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        payload_size: usize,
        source: Option<HeapObject>,
    ) -> Option<(HeapObject, bool)> {
        self.allocate_object_for_move_buffered(
            type_id,
            layout_id,
            gc_ref_mask,
            payload_size,
            source,
            None,
        )
    }

    /// A stopped collector may buffer entries for a newly reserved region;
    /// it must publish them before exposing the region to any heap index reader.
    /// A non-null `source` also requires all bitmap writers to be quiescent;
    /// only stopped minor evacuation supplies one.
    pub(super) fn allocate_object_for_move_buffered(
        &mut self,
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        payload_size: usize,
        source: Option<HeapObject>,
        pending: Option<&mut Vec<(usize, usize)>>,
    ) -> Option<(HeapObject, bool)> {
        assert!(
            !self.sweep_quarantined,
            "reclaimed spans are not reusable before sweep completion"
        );
        #[cfg(test)]
        {
            self.allocation_attempts += 1;
        }
        let total_size = GC_HEADER_SIZE.checked_add(payload_size)?;
        let span_size = total_size.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?;
        // The sweep-built index preserves address-ordered first fit without
        // scanning holes or shifting the remaining spans after every reuse.
        let mut reused = false;
        let offset = if let Some(offset) = self.free_spans.take(span_size) {
            reused = true;
            offset
        } else {
            let end = self.used.checked_add(span_size)?;
            if end > self.capacity {
                return None;
            }
            let offset = self.used;
            self.used = end;
            offset
        };

        self.largest_free_span = self.free_spans.largest();

        // SAFETY: the chosen span is exclusively owned by this allocation.
        let raw = unsafe { self.base.add(offset) };
        // Evacuation initializes the header and copies every payload byte
        // before the stopped collector publishes the destination. Zeroing the
        // same bytes first is redundant. A fresh region is already zeroed by
        // its allocator; eagerly touching a large scalar buffer again would
        // serialize page faults before parallel writers can fill their chunks.
        // Every later allocation clears its span, even after used returns to 0.
        if source.is_none() && !self.pristine {
            unsafe { std::ptr::write_bytes(raw, 0, span_size) };
            #[cfg(test)]
            {
                self.cleared_bytes += span_size;
            }
        }
        self.pristine = false;
        let object = if let Some(source) = source {
            debug_assert_eq!(source.size(), total_size);
            HeapObject::initialize_moved_at(raw, source, GC_GENERATION_OLD)
        } else {
            HeapObject::initialize_at(
                raw,
                total_size,
                type_id,
                layout_id,
                gc_ref_mask,
                GC_GENERATION_OLD,
            )
        }?;
        if let Some(pending) = pending {
            debug_assert!(!reused, "buffered promotion uses a fresh region");
            pending.push((offset, span_size));
        } else {
            self.allocations.insert(offset, span_size);
        }
        self.live_bytes = self.live_bytes.saturating_add(total_size);
        // Header initialization and black color precede publishing the start bit.
        // An epoch reader that observes the start may safely read immutable metadata.
        if GC_MARK_PHASE.load(Ordering::Acquire) != 0 {
            if source.is_some() {
                self.concurrent_marks
                    .set_stopped(offset / GC_REGION_MARK_GRANULE);
            } else {
                self.concurrent_marks.set(offset / GC_REGION_MARK_GRANULE);
            }
        }
        if source.is_some() {
            self.mark_bitmap.mark_stopped(offset);
        } else {
            self.mark_bitmap.mark(offset);
        }
        if self.sweep_pending {
            // Marking is closed, but this region has not yet been swept. New
            // allocations must survive its pending visit, including reused holes.
            object.begin_trace();
        }
        Some((object, reused))
    }

    /// Stopped evacuation into a fresh, exclusively owned promotion region.
    /// No free-span search, zeroing, or per-object tree insertion is needed.
    #[inline]
    pub(super) fn allocate_fresh_promotion(
        &mut self,
        source: HeapObject,
        size: usize,
        pending: &mut Vec<(usize, usize)>,
    ) -> Option<HeapObject> {
        debug_assert!(!self.sweep_quarantined && !self.sweep_pending);
        debug_assert_eq!(self.largest_free_span, 0);
        #[cfg(test)]
        {
            self.allocation_attempts += 1;
        }
        debug_assert_eq!(size, source.size());
        debug_assert!(size.is_multiple_of(GC_REGION_MARK_GRANULE));
        if size > self.capacity - self.used {
            return None;
        }
        let offset = self.used;
        // The validated young span is aligned and this cursor owns its tail.
        self.pristine = false;
        let object = HeapObject::initialize_moved_at(
            unsafe { self.base.add(offset) },
            source,
            GC_GENERATION_OLD,
        )?;
        self.used += size;
        self.live_bytes += size;
        pending.push((offset, size));
        if GC_MARK_PHASE.load(Ordering::Acquire) != 0 {
            self.concurrent_marks
                .set_stopped(offset / GC_REGION_MARK_GRANULE);
        }
        self.mark_bitmap.mark_stopped(offset);
        Some(object)
    }

    /// Reserve `[offset, offset + len)` for a per-thread allocation buffer,
    /// with `minimum <= len <= maximum`: the first free span of at least
    /// `minimum` bytes, else the bump tail. Returns `(offset, len, reused)`.
    pub(super) fn carve(&mut self, minimum: usize, maximum: usize) -> Option<(usize, usize, bool)> {
        assert!(
            !self.sweep_quarantined,
            "reclaimed spans are not reusable before sweep completion"
        );
        debug_assert!(minimum.is_multiple_of(GC_REGION_MARK_GRANULE));
        let maximum = maximum - maximum % GC_REGION_MARK_GRANULE;
        let carved = if let Some((offset, len)) = self.free_spans.take_up_to(minimum, maximum) {
            Some((offset, len, true))
        } else {
            let tail = self.capacity - self.used;
            (tail >= minimum).then(|| {
                let len = maximum.min(tail - tail % GC_REGION_MARK_GRANULE);
                let offset = self.used;
                self.used += len;
                (offset, len, false)
            })
        };
        self.largest_free_span = self.free_spans.largest();
        if carved.is_some() {
            self.pristine = false;
        }
        carved
    }

    /// Find the object whose payload starts at `address`, or whose payload
    /// contains it when `interior`. Exact lookups test the object-start bit in
    /// O(1); interior lookups use the ordered index and, for an address past
    /// every indexed object, the start bits of not yet indexed buffer objects.
    pub(super) fn object_for_address(&self, address: usize, interior: bool) -> Option<HeapObject> {
        if !self.contains(address) && (interior || address != self.end()) {
            return None;
        }
        let relative = address - self.start();
        let offset = if interior {
            match self.allocations.range(..=relative).next_back() {
                Some((&offset, &span)) if relative < offset + span => offset,
                indexed => {
                    // Only unindexed buffer objects (at most
                    // GC_TLAB_MAX_OBJECT_SIZE each) lie past the indexed one.
                    let floor = indexed
                        .map_or(0, |(&offset, &span)| offset + span)
                        .max(relative.saturating_sub(GC_TLAB_MAX_OBJECT_SIZE));
                    let header = relative.checked_sub(GC_HEADER_SIZE)?;
                    self.mark_bitmap.last_start_in(floor, header)?
                }
            }
        } else {
            let header = relative.checked_sub(GC_HEADER_SIZE)?;
            if !header.is_multiple_of(GC_REGION_MARK_GRANULE) || !self.mark_bitmap.is_marked(header)
            {
                return None;
            }
            header
        };
        // SAFETY: a start bit or index entry names a live object at this offset.
        let object = HeapObject::from_raw(unsafe { self.base.add(offset) }.cast())?;
        let payload = object.payload().as_ptr() as usize;
        let payload_end = object.as_ptr() as usize + object.size();
        ((!interior && payload == address)
            || (interior && address >= payload && address < payload_end))
            .then_some(object)
    }

    #[cfg(test)]
    pub(super) fn record_marked_object(&mut self, object: HeapObject) {
        let offset = object.as_ptr() as usize - self.start();
        self.mark_bitmap.mark(offset);
    }

    #[cfg(test)]
    pub(super) fn release_object(&mut self, object: HeapObject) {
        let offset = object.as_ptr() as usize - self.start();
        let Some(span_size) = self.allocations.remove(&offset) else {
            panic!(
                "willow gc: old object 0x{:x} is missing region allocation metadata",
                object.as_ptr() as usize
            );
        };
        self.live_bytes = self.live_bytes.saturating_sub(object.size());
        object.reclaim_in_place();
        self.mark_bitmap.unmark(offset);
        self.free_spans.push(RegionFreeSpan {
            offset,
            size: span_size,
        });
        self.coalesce_free_spans();
    }

    #[cfg(test)]
    pub(super) fn coalesce_free_spans(&mut self) {
        let mut spans: Vec<_> = self.free_spans.iter().copied().collect();
        spans.sort_unstable_by_key(|span| span.offset);
        let mut merged: Vec<RegionFreeSpan> = Vec::with_capacity(spans.len());
        for span in spans {
            if let Some(last) = merged.last_mut()
                && last.offset + last.size == span.offset
            {
                last.size += span.size;
                continue;
            }
            merged.push(span);
        }
        while merged
            .last()
            .is_some_and(|span| span.offset + span.size == self.used)
        {
            self.used = merged.pop().expect("tail span exists").offset;
        }
        self.largest_free_span = merged.iter().map(|span| span.size).max().unwrap_or(0);
        self.free_spans = FreeSpans::from(merged);
    }

    pub(super) fn available_span(&self) -> usize {
        self.largest_free_span.max(self.capacity - self.used)
    }

    pub(super) fn fragmentation_bytes(&self) -> usize {
        self.used.saturating_sub(self.live_bytes)
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static STORAGE_FAILURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static STORAGE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Allocate a validated, nonzero region layout. The test hook injects actual
/// storage failures without affecting Rust metadata allocations or other threads.
pub(super) unsafe fn allocate_region_storage(layout: Layout) -> *mut u8 {
    #[cfg(test)]
    {
        STORAGE_ATTEMPTS.set(STORAGE_ATTEMPTS.get() + 1);
        if STORAGE_FAILURES.get() != 0 {
            STORAGE_FAILURES.set(STORAGE_FAILURES.get() - 1);
            return std::ptr::null_mut();
        }
    }
    unsafe { alloc_zeroed(layout) }
}

impl Drop for OldRegion {
    fn drop(&mut self) {
        for &offset in self.allocations.keys() {
            // SAFETY: the region still owns each indexed header.
            HeapObject::from_raw(unsafe { self.base.add(offset) }.cast())
                .unwrap()
                .reclaim_in_place();
        }
        if !self.base.is_null() {
            // SAFETY: each region owns one block and `Drop` runs exactly once.
            unsafe { region_cache::free_region_storage(self.base, self.capacity) };
        }
    }
}

#[cfg(test)]
mod zero_initialization_tests {
    use super::*;

    fn assert_zero(object: HeapObject, bytes: usize) {
        let payload = unsafe { std::slice::from_raw_parts(object.payload().as_ptr(), bytes) };
        assert!(payload.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn fresh_and_reused_regions_preserve_zero_initialization() {
        let _guard = runtime_test_guard();
        for kind in [RegionKind::Old, RegionKind::LargeObject] {
            for bytes in [0, 8, 64, 512, 4096, 65536] {
                let mut region = OldRegion::new(kind, 131072).unwrap();
                assert!(region.pristine);
                let (first, _) = region.allocate_object(0, 0, 0, bytes).unwrap();
                assert!(!region.pristine);
                assert_zero(first, bytes);
                assert_eq!(region.cleared_bytes, 0);
                unsafe { std::ptr::write_bytes(first.payload().as_ptr(), 0xa5, bytes) };
                region.release_object(first);
                assert_eq!(region.used, 0);
                assert!(
                    !region.pristine,
                    "tail trimming must not restore pristine state"
                );
                let (second, _) = region.allocate_object(0, 0, 0, bytes).unwrap();
                assert_zero(second, bytes);
                assert_eq!(
                    region.cleared_bytes,
                    (GC_HEADER_SIZE + bytes).next_multiple_of(GC_REGION_MARK_GRANULE)
                );
                println!(
                    "kind={kind:?} bytes={bytes} fresh_clear_bytes=0 reused_clear_bytes={}",
                    region.cleared_bytes
                );
            }
        }
    }

    #[test]
    fn released_storage_reuse_clears_every_allocation() {
        for kind in [RegionKind::Old, RegionKind::LargeObject] {
            let mut released = OldRegion::new(kind, 4096).unwrap();
            let base = released.take_storage();
            drop(released);
            unsafe { std::ptr::write_bytes(base, 0xa5, 4096) };
            // SAFETY: released storage of exactly 4096 bytes, owned here.
            let mut region = unsafe { OldRegion::reuse(kind, 4096, base) };
            assert_eq!(region.start(), base as usize);
            assert!(!region.pristine);
            let bytes = 4096 - GC_HEADER_SIZE;
            let (object, _) = region.allocate_object(0, 0, 0, bytes).unwrap();
            assert_zero(object, bytes);
            assert_eq!(region.cleared_bytes, 4096);
        }
    }

    #[test]
    fn reclaimed_middle_hole_is_cleared() {
        let _guard = runtime_test_guard();
        let mut region = OldRegion::new(RegionKind::Old, 4096).unwrap();
        let (first, _) = region.allocate_object(0, 0, 0, 64).unwrap();
        let (_last, _) = region.allocate_object(0, 0, 0, 64).unwrap();
        unsafe { std::ptr::write_bytes(first.payload().as_ptr(), 0xa5, 64) };
        region.release_object(first);
        let (replacement, reused) = region.allocate_object(0, 0, 0, 64).unwrap();
        assert!(reused);
        assert_zero(replacement, 64);
    }

    #[test]
    fn lab_carving_and_promotion_consume_pristine_state() {
        let _guard = runtime_test_guard();
        let mut carved = OldRegion::new(RegionKind::Old, 4096).unwrap();
        assert!(carved.carve(4096, 4096).is_some());
        assert!(!carved.pristine);
        // Model a dirty retired LAB whose whole span became the bump tail.
        unsafe { std::ptr::write_bytes(carved.base, 0xa5, carved.capacity) };
        carved.used = 0;
        let (object, _) = carved.allocate_object(0, 0, 0, 64).unwrap();
        assert_zero(object, 64);

        let mut source_region = OldRegion::new(RegionKind::Old, 4096).unwrap();
        let (source, _) = source_region.allocate_object(0, 0, 0, 64).unwrap();
        unsafe { std::ptr::write_bytes(source.payload().as_ptr(), 0xa5, 64) };
        for buffered in [false, true] {
            let mut destination = OldRegion::new(RegionKind::Old, 4096).unwrap();
            let copied = if buffered {
                let mut pending = Vec::new();
                let copied = destination
                    .allocate_fresh_promotion(source, source.size(), &mut pending)
                    .unwrap();
                destination.allocations.extend(pending);
                copied
            } else {
                destination
                    .allocate_object_for_move(0, 0, 0, 64, Some(source))
                    .unwrap()
                    .0
            };
            // Evacuation's caller copies payload bytes after the region has
            // initialized the destination header and before publication.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.payload().as_ptr(),
                    copied.payload().as_ptr(),
                    64,
                )
            };
            assert!(!destination.pristine);
            assert_eq!(unsafe { copied.payload().as_ptr().read() }, 0xa5);
            destination.release_object(copied);
            let (replacement, _) = destination.allocate_object(0, 0, 0, 64).unwrap();
            assert_zero(replacement, 64);
        }
    }
}
