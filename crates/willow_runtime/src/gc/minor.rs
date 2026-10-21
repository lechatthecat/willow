//! Stop-the-world minor collection, survivor copying, and tenuring.

use std::alloc::{Layout, dealloc};
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;

use super::{
    DropFn, GC_GENERATION_OLD, GC_GENERATION_YOUNG, GC_HEADER_SIZE, GC_REGION_MARK_GRANULE,
    GcHeader, GcPayload, GcState, HeapObject, MinorRegistrySnapshot, RegionKind, RootSlot,
    allocate_old_region_object_for_move, append_object_reference_slots, append_parked_roots,
    foreign_root_stack_owner_active, foreign_root_stack_owner_active_locked, index_retired_chunk,
    minor_registry_snapshot, minor_stack_roots, retire_all_tlabs_unindexed, runtime,
    runtime_root_slots, tlab_payload_generation, verify_old_region_metadata, verify_remembered_set,
    willow_gc_safepoint, with_stw,
};

/// Collector-private address keys use the shared folded multiply; SipHash
/// dominated minor collection time (willow-8hq4.16).
pub(super) use crate::id_hash::IdHasher as AddressHasher;
type AddressMap<V> = HashMap<usize, V, BuildHasherDefault<AddressHasher>>;
type AddressSet = HashSet<usize, BuildHasherDefault<AddressHasher>>;

#[cfg(test)]
#[path = "minor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "minor_deferral_tests.rs"]
mod deferral_tests;

#[cfg(test)]
#[path = "minor_overlap_tests.rs"]
mod overlap_tests;

#[cfg(test)]
thread_local! {
    static PAGE_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SNAPSHOT_DEDUP_KEYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct MinorCollector<'a> {
    work: crate::gc_telemetry::MarkWork,
    stop_work: &'a mut crate::gc_telemetry::stops::StopWorkV2,
    state: &'a mut GcState,
    survivor_destination: Option<usize>,
    promotion_destination: Option<usize>,
    promotion_entries: Vec<(usize, usize)>,
    buffer_promotion_entries: bool,
    /// Chunks at indices below this existed before the cycle; later ones are
    /// survivor destinations allocated by this cycle.
    source_chunks: usize,
    chunk_pages: AddressMap<[usize; 2]>,
    last_chunk: std::cell::Cell<Option<usize>>,
    forwarded_objects: usize,
    forwarded_per_chunk: Vec<usize>,
    published: Vec<usize>,
    // Old edges from inline generated layouts scanned by this minor. Publish
    // after all relocated targets have their epoch marks, so new-to-new edges
    // do not create duplicate trace jobs.
    major_old_edges: AddressSet,
    already_queued: AddressSet,
    worklist: Vec<(HeapObject, bool)>,
    // Reuse storage across objects; trace hooks retain their existing Vec ABI.
    reference_slots: Vec<*mut *mut u8>,
    inline_types: [Option<(u32, bool, bool)>; 4],
    scanned: AddressSet,
    registry: std::sync::Arc<MinorRegistrySnapshot>,
}

impl Drop for MinorCollector<'_> {
    fn drop(&mut self) {
        self.release_promotion_destination();
    }
}

impl<'a> MinorCollector<'a> {
    fn new(
        state: &'a mut GcState,
        registry: std::sync::Arc<MinorRegistrySnapshot>,
        stop_work: &'a mut crate::gc_telemetry::stops::StopWorkV2,
    ) -> Self {
        debug_assert!(
            state
                .tlab_chunks
                .iter()
                .all(|chunk| chunk.owner_state.is_none()),
            "minor collection requires retired TLABs"
        );
        // A newly retired chunk is validated and indexed only when the cycle
        // first finds a young object in it (`young_source`) or must walk it in
        // the sweep; the collector wrote every survivor header. Young
        // membership is answered by the chunk index plus start bitmap, so no
        // per-object index is built for the (mostly dead) nursery.
        let source_chunks = state.tlab_chunks.len();
        let mut collector = Self {
            work: crate::gc_telemetry::MarkWork::default(),
            stop_work,
            state,
            survivor_destination: None,
            promotion_destination: None,
            promotion_entries: Vec::new(),
            buffer_promotion_entries: false,
            source_chunks,
            chunk_pages: AddressMap::default(),
            last_chunk: std::cell::Cell::new(None),
            forwarded_objects: 0,
            forwarded_per_chunk: vec![0; source_chunks],
            published: Vec::new(),
            major_old_edges: AddressSet::default(),
            already_queued: AddressSet::default(),
            worklist: Vec::new(),
            reference_slots: Vec::new(),
            inline_types: [None; 4],
            scanned: AddressSet::default(),
            registry,
        };
        for index in 0..source_chunks {
            collector.index_chunk(index);
        }
        collector
    }

    fn index_chunk(&mut self, index: usize) {
        let chunk = &self.state.tlab_chunks[index];
        let start = chunk.base as usize;
        let page_bytes = super::GC_TLAB_CHUNK_SIZE;
        for page in start / page_bytes..=(start + chunk.capacity - 1) / page_bytes {
            let candidates = self.chunk_pages.entry(page).or_insert([usize::MAX; 2]);
            *candidates
                .iter_mut()
                .find(|slot| **slot == usize::MAX)
                .expect("disjoint fixed-size chunks overlap at most two page candidates") = index;
        }
    }

    // This cycle's fixed-size chunks have at most two candidates per page.
    // Expected O(1) lookup; no ordered-region search for every live child.
    // The map is used only before sweep compacts the chunk vector.
    fn chunk_index(&self, header: usize) -> Option<usize> {
        // Sequential graph edges usually stay in the same retired chunk. The
        // range is rechecked against current `used`; indices stay stable until
        // sweep, after which this collector's page index is no longer queried.
        if let Some(index) = self.last_chunk.get() {
            let chunk = &self.state.tlab_chunks[index];
            let base = chunk.base as usize;
            if header >= base && header < base + chunk.used {
                return Some(index);
            }
        }
        #[cfg(test)]
        PAGE_QUERIES.set(PAGE_QUERIES.get() + 1);
        let index = self
            .chunk_pages
            .get(&(header / super::GC_TLAB_CHUNK_SIZE))?
            .iter()
            .copied()
            .filter(|&index| index != usize::MAX)
            .find(|&index| {
                let chunk = &self.state.tlab_chunks[index];
                let base = chunk.base as usize;
                header >= base && header < base + chunk.used
            });
        self.last_chunk.set(index);
        index
    }

    fn chunk_generation(&self, address: usize) -> Option<u8> {
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        let index = self.chunk_index(header)?;
        super::object_in_retired_chunk(&self.state.tlab_chunks[index], address, false)
            .map(|object| object.generation())
    }

    /// The young object whose payload starts at `address`, if it lies in a
    /// nursery or survivor chunk that existed when this cycle began. Chunks
    /// created by `allocate_survivor` are destinations, never sources.
    /// Expected O(1) for the bounded page index and start bit,
    /// plus the one-time validation of a chunk not yet indexed.
    fn young_source(&mut self, address: usize) -> Option<HeapObject> {
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        let index = self.chunk_index(header)?;
        if index >= self.source_chunks {
            return None;
        }
        let chunk = &mut self.state.tlab_chunks[index];
        let offset = header - chunk.base as usize;
        if chunk.kind == RegionKind::Pinned
            || offset >= chunk.used
            || !offset.is_multiple_of(GC_REGION_MARK_GRANULE)
            || !chunk.mark_bitmap.is_marked(offset)
        {
            return None;
        }
        // A chunk holding a reachable object is walked by the sweep anyway;
        // validate its start bits before trusting one (willow-8hq4.21).
        let indexed = index_retired_chunk(chunk) as u64;
        self.stop_work.metadata_objects += indexed;
        self.stop_work.metadata_bytes += indexed * GC_HEADER_SIZE as u64;
        // SAFETY: a validated start bit inside the retired prefix names a header.
        let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
        (object.allocated() && object.generation() == GC_GENERATION_YOUNG).then_some(object)
    }

    /// Retain a young object in place by promoting it where it lies. Only a
    /// hard memory budget uses this for young objects: a root published from
    /// runtime code that cannot be deferred, or a nursery object left without
    /// evacuation space. Root slots are rewritten instead (`scan_root_slot`).
    /// Children of a retained object can still move; its reference slots are
    /// updated when it is scanned.
    fn pin_root(&mut self, payload: *mut u8, source: PinSource) {
        if payload.is_null() {
            return;
        }
        let address = payload as usize;
        if !self.scanned.insert(address) {
            return;
        }
        if let Some(object) = self.young_source(address) {
            if verify_no_pin() {
                crate::failure::fatal_invariant(&format!(
                    "WILLOW_GC_VERIFY_NO_PIN: minor collection pinned a young object ({})",
                    source.describe()
                ));
            }
            object.set_generation(GC_GENERATION_OLD);
            if self.state.concurrent_cycle.is_some() {
                self.published.push(address);
            }
            self.state.survivor_stats.pinned_promotions += 1;
            let size = object.size();
            let stats = &mut self.state.survivor_stats;
            stats.pinned_bytes = stats.pinned_bytes.saturating_add(size as u64);
            stats.active_pin_bytes += size as u64;
            stats.active_pins += 1;
            stats.pin_reasons[source as usize] += 1;
            self.state.pin_records.insert(
                address,
                crate::gc_telemetry::GcPinRecord {
                    owner_allocation: address,
                    reason: source.describe(),
                    minor_epoch: self.state.minor_collections,
                    major_epoch: self.state.major_collections,
                    bytes: size as u64,
                },
            );
            self.state.young_allocated_bytes =
                self.state.young_allocated_bytes.saturating_sub(size);
            self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
            self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
            self.worklist.push((object, true));
            return;
        }
        if let Some(payload) = GcPayload::from_raw(payload) {
            let object = HeapObject::from_payload(payload);
            if object.allocated() && object.generation() == GC_GENERATION_OLD {
                self.worklist.push((object, true));
                return;
            }
        }
        #[cfg(debug_assertions)]
        panic!("willow gc: invalid root 0x{address:x} during minor collection");
    }

    /// A per-cycle bump cursor: destinations are never evacuation sources in
    /// this cycle, and no search over old survivor chunks is needed per copy.
    fn allocate_survivor(
        &mut self,
        source: HeapObject,
        size: usize,
        age: u8,
    ) -> Option<HeapObject> {
        use super::{BumpChunk, GC_TLAB_CHUNK_SIZE, RegionMarkBitmap};
        if size > GC_TLAB_CHUNK_SIZE {
            return None;
        }
        if self.survivor_destination.is_none_or(|index| {
            let chunk = &self.state.tlab_chunks[index];
            chunk.capacity - chunk.used < size
        }) {
            if !super::can_reserve(self.state, GC_TLAB_CHUNK_SIZE) {
                return None;
            }
            let layout =
                Layout::from_size_align(GC_TLAB_CHUNK_SIZE, std::mem::align_of::<GcHeader>())
                    .ok()?;
            // SAFETY: valid aligned layout, owned until the common chunk cleanup.
            let base = unsafe { super::allocate_region_storage(layout) };
            if base.is_null() {
                return None;
            }
            let index = self.state.tlab_chunks.len();
            self.state.tlab_addresses.insert(base as usize, index);
            self.state.tlab_chunks.push(BumpChunk {
                base,
                capacity: GC_TLAB_CHUNK_SIZE,
                used: 0,
                owner_state: None,
                kind: RegionKind::Survivor,
                live_bytes: 0,
                mark_bitmap: RegionMarkBitmap::new(GC_TLAB_CHUNK_SIZE),
                concurrent_marks: std::sync::Arc::new(
                    super::concurrent_bitmap::ConcurrentMarkBits::new(
                        GC_TLAB_CHUNK_SIZE / GC_REGION_MARK_GRANULE,
                    ),
                ),
                header_offsets: Vec::new(),
            });
            self.index_chunk(index);
            self.state.tlab_reserved_bytes += GC_TLAB_CHUNK_SIZE;
            super::note_reservation_growth(self.state);
            self.state.survivor_stats.survivor_space_reserved += GC_TLAB_CHUNK_SIZE as u64;
            self.survivor_destination = Some(index);
        }
        let chunk = &mut self.state.tlab_chunks[self.survivor_destination.unwrap()];
        // SAFETY: this exclusive collector cursor owns sufficient aligned space.
        let target = HeapObject::initialize_moved_at(
            unsafe { chunk.base.add(chunk.used) },
            source,
            GC_GENERATION_YOUNG,
        )?;
        unsafe {
            (*target.as_ptr()).age = age;
        }
        chunk
            .header_offsets
            .push(u16::try_from(chunk.used).expect("chunk offset fits u16"));
        chunk.mark_bitmap.mark_stopped(chunk.used);
        chunk.used += size;
        chunk.live_bytes += size;
        self.state.survivor_stats.survivor_space_live += size as u64;
        self.state.allocated_bytes += size;
        self.state.young_allocated_bytes += size;
        Some(target)
    }

    fn release_promotion_destination(&mut self) {
        if let Some(index) = self.promotion_destination.take() {
            if self.buffer_promotion_entries {
                // Bulk-build sorted starts once per fresh region rather than
                // updating its allocation tree on every promotion.
                // Include the ordinary allocator's first entry in that same
                // build. Collect-then-append would build and discard a second
                // BTreeMap for every fresh promotion region.
                let allocations = &mut self.state.old_regions[index].allocations;
                debug_assert_eq!(allocations.len(), 1);
                *allocations = std::mem::take(allocations)
                    .into_iter()
                    .chain(self.promotion_entries.drain(..))
                    .collect();
                self.buffer_promotion_entries = false;
            }
            super::index_old_region(self.state, index);
        }
    }

    /// The stopped collector exclusively fills one regular region. Re-key its
    /// candidate entry once per region rather than once per promoted object.
    fn allocate_promotion(
        &mut self,
        source: HeapObject,
        metadata: super::raw_heap::TraceMetadata,
    ) -> Option<HeapObject> {
        let size = GC_HEADER_SIZE + metadata.payload_size;
        if let Some(index) = self.promotion_destination {
            let allocated = if self.buffer_promotion_entries {
                self.state.old_regions[index]
                    .allocate_fresh_promotion(source, size, &mut self.promotion_entries)
                    .map(|object| (object, false))
            } else {
                self.state.old_regions[index].allocate_object_for_move_buffered(
                    metadata.type_id,
                    metadata.layout_id,
                    metadata.gc_ref_mask,
                    metadata.payload_size,
                    Some(source),
                    None,
                )
            };
            if let Some((object, reused)) = allocated {
                super::account_old_region_allocation(self.state, size, reused, false);
                return Some(object);
            }
            self.release_promotion_destination();
        }
        let previous_regions = self.state.old_regions.len();
        let object = allocate_old_region_object_for_move(
            self.state,
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
            false,
            Some(source),
        )?;
        let index = self
            .state
            .old_addresses
            .candidate(object.as_ptr() as usize)
            .expect("promoted object has an old region");
        if self.state.old_regions[index].kind == RegionKind::Old {
            let available = self.state.old_regions[index].available_span();
            self.state.old_region_candidates.remove(&(available, index));
            self.promotion_destination = Some(index);
            self.buffer_promotion_entries = index >= previous_regions;
        }
        Some(object)
    }

    fn evacuate(&mut self, payload: *mut u8) -> *mut u8 {
        if payload.is_null() {
            return payload;
        }
        // Reference slots supplied by generated layouts and registered trace
        // hooks contain managed payloads. Most entries of a growing old array
        // already point to old objects: reject them before probing the nursery
        // page index. This is the same header proof used after scan_slot below.
        let object = HeapObject::from_payload(GcPayload::from_raw(payload).unwrap());
        if object.generation() == GC_GENERATION_OLD {
            return payload;
        }
        // This slot already proves an exact allocated payload. Locate its
        // chunk only to distinguish pre-cycle sources from survivor targets;
        // root/publication boundaries still use young_source's full checks.
        let Some(index) = self.chunk_index(object.as_ptr() as usize) else {
            return payload;
        };
        if index >= self.source_chunks {
            return payload;
        }
        let chunk = &mut self.state.tlab_chunks[index];
        if chunk.kind == RegionKind::Pinned {
            return payload;
        }
        if chunk.needs_index() {
            let indexed = index_retired_chunk(chunk) as u64;
            self.stop_work.metadata_objects += indexed;
            self.stop_work.metadata_bytes += indexed * GC_HEADER_SIZE as u64;
        }
        debug_assert!(object.allocated());
        debug_assert!(
            chunk
                .mark_bitmap
                .is_marked(object.as_ptr() as usize - chunk.base as usize)
        );
        self.evacuate_source(object, payload)
    }

    /// `source` was validated once by the caller for this stopped cycle.
    fn evacuate_source(&mut self, source: HeapObject, payload: *mut u8) -> *mut u8 {
        if let Some(forwarded) = source.forwarded_to() {
            return forwarded.as_ptr();
        }

        let metadata = source.trace_metadata();
        let size = GC_HEADER_SIZE + metadata.payload_size;
        // young_source validated this exact source and cached its stable index.
        let source_chunk = self.last_chunk.get().expect("validated young source chunk");
        // SAFETY: the source is a validated young allocation under STW.
        let age = unsafe { (*source.as_ptr()).age }.saturating_add(1);
        let target = if age < self.state.tenuring.age() {
            self.allocate_survivor(source, size, age)
        } else {
            self.allocate_promotion(source, metadata)
        };
        let Some(target) = target else {
            if self.state.memory_limit_bytes.is_some() {
                // A hard region budget must not require extra evacuation
                // storage. Retain this nursery object in place, using the same
                // promotion path as value roots, and keep tracing it.
                self.pin_root(payload, PinSource::MemoryBudget);
                return payload;
            }
            crate::gc_telemetry::failure_record::emit(
                "evacuation_allocation",
                "minor evacuation allocation failed without a region budget",
            );
            std::process::abort();
        };
        // SAFETY: source and target are distinct allocations with identical
        // payload sizes. Header/list metadata remains owned by the collector.
        unsafe {
            let from = source.payload().as_ptr();
            let to = target.payload().as_ptr();
            // Constant-sized scalar/pair payloads compile to direct loads and
            // stores instead of a general memcpy call for every tiny object.
            match metadata.payload_size {
                0 => {}
                8 => std::ptr::copy_nonoverlapping(from, to, 8),
                16 => std::ptr::copy_nonoverlapping(from, to, 16),
                size => std::ptr::copy_nonoverlapping(from, to, size),
            }
        }
        if target.generation() == GC_GENERATION_OLD {
            self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
            self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
            self.state.survivor_stats.tenured_objects += 1;
            self.state.survivor_stats.tenured_bytes += size as u64;
        } else {
            self.state.survivor_stats.survivor_copies += 1;
            self.state.survivor_stats.survivor_bytes += size as u64;
        }
        self.state.moved_objects = self.state.moved_objects.saturating_add(1);
        let forwarded = target.payload().as_ptr();
        source.forward_to(target);
        self.forwarded_objects += 1;
        self.forwarded_per_chunk[source_chunk] += 1;
        if self.state.concurrent_cycle.is_some() {
            self.published.push(forwarded as usize);
        }
        if metadata.gc_ref_mask == 0 && self.inline_policy(metadata).0 {
            // A generated leaf has no slots to rewrite. Credit its scan here
            // instead of enqueueing and decoding the same header again.
            self.work.frame(
                metadata.layout_id,
                metadata.type_id,
                metadata.payload_size,
                0,
            );
            self.work.object(size, 0);
        } else {
            self.worklist.push((target, false));
        }
        forwarded
    }

    fn scan_slot(&mut self, slot: *mut *mut u8) {
        if slot.is_null() {
            return;
        }
        // SAFETY: slots come from layout masks or registered mutable trace hooks.
        let old = unsafe { *slot };
        let new = self.evacuate(old);
        if new != old {
            // SAFETY: minor collection owns all heap mutation under STW.
            unsafe { *slot = new };
        }
    }

    /// Evacuate a root slot's young referent and rewrite the slot (willow-9tls.9).
    /// The slot is the holder's only copy across this collection, so the
    /// referent need not stay in place. A slot already rewritten earlier in
    /// this cycle (an aliased registration) names a destination and is left
    /// alone; old referents are traced as before.
    fn scan_root_slot(&mut self, slot: *mut *mut u8) {
        // SAFETY: published root slots stay valid while their owners are stopped.
        let value = unsafe { *slot };
        if value.is_null() {
            return;
        }
        if let Some(source) = self.young_source(value as usize) {
            let updated = self.evacuate_source(source, value);
            // SAFETY: the published root slot is writable under this stop.
            if updated != value {
                unsafe { *slot = updated };
            }
            return;
        }
        if !self.is_destination(value as usize) {
            self.pin_root(value, PinSource::OldOrForeign);
        }
    }

    /// Whether `address` names a destination, expected O(1).
    fn is_destination(&self, address: usize) -> bool {
        address
            .checked_sub(GC_HEADER_SIZE)
            .and_then(|header| self.chunk_index(header))
            .is_some_and(|index| index >= self.source_chunks)
    }

    // Both registries and the current major epoch are immutable throughout
    // this stopped minor. Cache only type dispatch, never payloads or liveness.
    fn inline_policy(&mut self, metadata: super::raw_heap::TraceMetadata) -> (bool, bool) {
        let index = metadata.type_id as usize % self.inline_types.len();
        if let Some((id, inline, major)) = self.inline_types[index]
            && id == metadata.type_id
        {
            return (inline, major);
        }
        let inline = metadata.type_id != willow_abi::GC_BITMAP_TYPE_ID
            && !self.registry.trace.contains_key(&metadata.type_id);
        let major = inline
            && self
                .state
                .concurrent_cycle
                .as_ref()
                .is_some_and(|cycle| cycle.generated_inline(metadata));
        self.inline_types[index] = Some((metadata.type_id, inline, major));
        (inline, major)
    }

    fn scan_object(&mut self, object: HeapObject) {
        let address = object.payload().as_ptr() as usize;
        if !object.allocated() {
            return;
        }
        let metadata = object.trace_metadata();
        self.work.frame(
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
        );
        let (inline, scanned_for_major) = self.inline_policy(metadata);
        let old_owner = object.generation() == GC_GENERATION_OLD;
        let mut has_young_child = false;
        if inline {
            let words = (metadata.payload_size / super::GC_STORAGE_WORD_BYTES).min(64);
            let mut mask =
                metadata.gc_ref_mask & u64::MAX.checked_shr((64 - words) as u32).unwrap_or(0);
            self.work.object(
                GC_HEADER_SIZE + metadata.payload_size,
                mask.count_ones() as usize,
            );
            while mask != 0 {
                let index = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                has_young_child |= self.scan_reference_slot(
                    object.payload_slot(index),
                    scanned_for_major,
                    old_owner,
                );
            }
        } else if let Some((first, count)) = (metadata.gc_ref_mask == 0)
            .then(|| unsafe {
                crate::array::minor_reference_range(
                    metadata.type_id,
                    self.registry.trace.get(&metadata.type_id).copied(),
                    object.payload().as_ptr(),
                )
            })
            .flatten()
        {
            self.work
                .object(GC_HEADER_SIZE + metadata.payload_size, count);
            for index in 0..count {
                // Built-in buffer length bounds this contiguous slot range.
                has_young_child |=
                    self.scan_reference_slot(unsafe { first.add(index) }, false, old_owner);
            }
        } else {
            let mut slots = std::mem::take(&mut self.reference_slots);
            append_object_reference_slots(object, &self.registry.trace, &mut slots);
            self.work.object(
                GC_HEADER_SIZE + metadata.payload_size,
                slots.iter().filter(|slot| !slot.is_null()).count(),
            );
            for &slot in &slots {
                has_young_child |= self.scan_reference_slot(slot, scanned_for_major, old_owner);
            }
            slots.clear();
            self.reference_slots = slots;
        }
        if has_young_child {
            super::barrier::remember_owner(self.state, address);
        }
    }

    fn scan_reference_slot(
        &mut self,
        slot: *mut *mut u8,
        scanned_for_major: bool,
        old_owner: bool,
    ) -> bool {
        if slot.is_null() {
            return false;
        }
        if scanned_for_major {
            // Record old children before evacuation; newly promoted children
            // are credited and published by this stopped minor itself.
            let child = unsafe { *slot };
            if let Some(payload) = GcPayload::from_raw(child)
                && HeapObject::from_payload(payload).generation() == GC_GENERATION_OLD
            {
                self.major_old_edges.insert(child as usize);
            }
        }
        self.scan_slot(slot);
        if old_owner {
            let child = unsafe { *slot };
            if let Some(payload) = GcPayload::from_raw(child) {
                return HeapObject::from_payload(payload).generation() == GC_GENERATION_YOUNG;
            }
        }
        false
    }

    fn snapshot_roots(&self) -> Vec<*mut u8> {
        let mut mark_roots = Vec::new();
        if let Some(cycle) = self.state.concurrent_cycle.as_ref() {
            assert_eq!(
                cycle
                    .active_drains
                    .load(std::sync::atomic::Ordering::Acquire),
                0
            );
            let mut seen = AddressSet::default();
            let mut retain = |address, transport| {
                if self.chunk_generation(address) == Some(GC_GENERATION_YOUNG)
                    && if transport {
                        seen.insert(address)
                    } else {
                        !seen.contains(&address)
                    }
                {
                    mark_roots.push(address as *mut u8);
                }
            };
            cycle.queue.remap_stopped(|object| {
                retain(object.addr(), true);
                Some(object)
            });
            for &address in cycle.deferred.lock().unwrap().iter() {
                retain(address, true);
            }
            for &address in cycle.unindexed.lock().unwrap().iter() {
                retain(address, true);
            }
            for chunk in &self.state.tlab_chunks {
                if chunk.kind == RegionKind::Pinned {
                    continue;
                }
                let bits = if cycle.objects.contains_extent(chunk.base as usize) {
                    &chunk.concurrent_marks
                } else {
                    // Post-capture nursery extents inherit black allocation. Keep
                    // their starts alive through relocation before making them
                    // visible to the old epoch's explicit bitmap/index.
                    &chunk.mark_bitmap.bits
                };
                bits.for_each_set(|bit| {
                    let address =
                        chunk.base as usize + bit * GC_REGION_MARK_GRANULE + GC_HEADER_SIZE;
                    // Bits are unique within disjoint extents. Only queued
                    // transports need a dedup entry; never insert every marked
                    // survivor into a second per-object hash table.
                    retain(address, false);
                });
            }
            #[cfg(test)]
            SNAPSHOT_DEDUP_KEYS.set(seen.len());
        }
        mark_roots
    }

    fn run(
        mut self,
        roots: MinorRoots,
        remembered: super::remembered::RememberedSet,
    ) -> (usize, crate::gc_telemetry::MarkWork) {
        let started = std::time::Instant::now();
        self.work.remembered_owners = remembered.len() as u64;
        let cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
        let evacuation = crate::gc_telemetry::measurement::Span::new(
            crate::gc_telemetry::measurement::Phase::Evacuation,
        );
        let root_scan = crate::gc_telemetry::measurement::Span::new(
            crate::gc_telemetry::measurement::Phase::Roots,
        );
        let count = roots.slots.len() + roots.values.len();
        self.work.root_scan_bytes = crate::gc_telemetry::MarkWork::roots(count).root_scan_bytes;
        self.stop_work.root_values += count as u64;
        // Value roots cannot be rewritten, so retain them in place before any
        // slot evacuates: a slot aliasing one of them then observes an old
        // object and keeps its value.
        // Explicit loans take attribution precedence over runtime-stack aliases.
        for &(root, source) in &roots.values {
            if source == PinSource::ScopedLoan {
                self.pin_root(root, source);
            }
        }
        for (root, source) in roots.values {
            if source != PinSource::ScopedLoan {
                self.pin_root(root, source);
            }
        }
        for owner in remembered {
            // The set was taken; `scan_object` re-remembers owners that keep
            // a young child after this collection.
            HeapObject::from_payload(GcPayload::from_raw(owner as *mut u8).unwrap())
                .set_remembered(false);
            self.pin_root(owner as *mut u8, PinSource::RememberedOwner);
        }
        for slot in roots.slots {
            self.scan_root_slot(slot);
        }
        drop(root_scan);
        while let Some((object, root)) = self.worklist.pop() {
            // Evacuation queues each new destination once. A repeated root slot
            // may also pin a promoted destination; the root entry owns its scan.
            // Keep the dedup set proportional to roots/pins, not all survivors.
            if !root && self.scanned.contains(&(object.payload().as_ptr() as usize)) {
                continue;
            }
            self.scan_object(object);
        }

        self.release_promotion_destination();

        // All referents have moved, but source headers remain valid until sweep.
        // Translate every transport before freeing storage, including legacy and
        // not-yet-indexed work. Old/pinned referents keep their identity.
        if let Some(cycle) = self.state.concurrent_cycle.clone() {
            let remap = |address: usize| -> Option<usize> {
                if self.chunk_generation(address) == Some(GC_GENERATION_YOUNG)
                    && !self.is_destination(address)
                {
                    let source = HeapObject::from_payload(GcPayload::from_raw(address as *mut u8)?);
                    return source.forwarded_to().map(|new| new.as_ptr() as usize);
                }
                Some(address)
            };
            // Only queue-owned addresses need a dedup entry. Building the
            // complement from every copied object adds an avoidable O(V) table
            // even when the old marker queue is empty.
            let mut already_queued = AddressSet::default();
            cycle.queue.remap_stopped(|object| {
                let address = remap(object.addr())?;
                already_queued.insert(address);
                crate::gc_mark_queue::ObjectRef::from_addr(address)
            });
            cycle.remap_recovery_native_retries(remap);
            let mut deferred = cycle.deferred.lock().unwrap();
            *deferred = deferred.iter().copied().filter_map(&remap).collect();
            let mut unindexed = cycle.unindexed.lock().unwrap();
            *unindexed = unindexed.iter().copied().filter_map(remap).collect();
            self.already_queued = already_queued;
        }
        drop(evacuation);
        let cpu = cpu_start
            .zip(crate::gc_telemetry::workers::thread_cpu_ns())
            .and_then(|(a, b)| b.checked_sub(a));
        self.work.cpu_ns = cpu.unwrap_or(0);
        self.work.cpu_incomplete = cpu.is_none();

        self.work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
        crate::gc_telemetry::milestones::reached(crate::gc_telemetry::GcStage::MarkComplete);
        let mut reclaimed_bytes = 0usize;
        let mut reclaimed_young = 0u64;
        // Nursery runs are usually one type; skip the registry probe for them.
        let mut last_type: Option<(u32, Option<DropFn>)> = None;
        let poison_moved = cfg!(debug_assertions) || super::gc_stress_enabled("relocate");
        self.state.survivor_stats.survivor_space_reserved = 0;
        self.state.survivor_stats.survivor_space_live = 0;
        let mut chunk_identities: Vec<_> = (0..self.state.tlab_chunks.len()).collect();
        let mut chunk_positions = vec![None; self.state.tlab_chunks.len()];
        let mut chunk_index = 0usize;
        while chunk_index < self.state.tlab_chunks.len() {
            // No allocation in an already-pinned chunk changes during a minor.
            // Keep its bitmap/live bytes and its identity for address remapping.
            if self.state.tlab_chunks[chunk_index].kind == RegionKind::Pinned {
                chunk_index += 1;
                continue;
            }
            let base = self.state.tlab_chunks[chunk_index].base;
            let used = self.state.tlab_chunks[chunk_index].used;
            let identity = chunk_identities[chunk_index];
            let source = identity < self.source_chunks;
            if source
                && !poison_moved
                && self.forwarded_per_chunk[identity] != 0
                && self.forwarded_per_chunk[identity]
                    == self.state.tlab_chunks[chunk_index].header_offsets.len()
            {
                // Every validated header in this chunk was forwarded exactly
                // once. Descriptor ownership moved with each destination;
                // no source owns a drop or descriptor release. Discard the
                // whole prefix without decoding all forwarded headers again.
                let objects = self.forwarded_per_chunk[identity] as u64;
                self.stop_work.swept_objects += objects;
                reclaimed_young += objects;
                self.state.allocated_bytes = self.state.allocated_bytes.saturating_sub(used);
                self.state.young_allocated_bytes =
                    self.state.young_allocated_bytes.saturating_sub(used);
                reclaimed_bytes = reclaimed_bytes.saturating_add(used);
                self.release_chunk(chunk_index, &mut chunk_identities);
                continue;
            }
            if self.state.tlab_chunks[chunk_index].needs_index() {
                // Marking never found a young object here, or `young_source`
                // would have indexed the chunk: every object in it is dead
                // and young (willow-8hq4.21).
                debug_assert!(source, "survivor destinations are indexed");
                if !self.may_drop(chunk_index) {
                    // Nothing to run per object: account the whole chunk from
                    // its bump prefix and start-bit popcount, O(chunk words).
                    let objects = self.state.tlab_chunks[chunk_index].mark_bitmap.bits.count();
                    #[cfg(debug_assertions)]
                    self.verify_dead_chunk(chunk_index);
                    self.stop_work.swept_objects += objects as u64;
                    reclaimed_young += objects as u64;
                    self.state.allocated_bytes = self.state.allocated_bytes.saturating_sub(used);
                    self.state.young_allocated_bytes =
                        self.state.young_allocated_bytes.saturating_sub(used);
                    reclaimed_bytes = reclaimed_bytes.saturating_add(used);
                    self.release_chunk(chunk_index, &mut chunk_identities);
                    continue;
                }
                // Drop hooks need the per-object walk; validate it first.
                let indexed = index_retired_chunk(&mut self.state.tlab_chunks[chunk_index]) as u64;
                self.stop_work.metadata_objects += indexed;
                self.stop_work.metadata_bytes += indexed * GC_HEADER_SIZE as u64;
            }
            let mut has_allocated = false;
            let mut has_young = false;
            let mut live_bytes = 0usize;
            self.state.tlab_chunks[chunk_index].mark_bitmap.clear();
            // Retirement and survivor copying index every header, so visit the
            // index instead of chasing `offset += size`; independent header
            // loads overlap instead of serializing on the previous header.
            let offsets = std::mem::take(&mut self.state.tlab_chunks[chunk_index].header_offsets);
            debug_assert!(
                offsets.last().is_none_or(|&last| {
                    // SAFETY: indexed offsets name headers in the retired prefix.
                    let object =
                        HeapObject::from_raw(unsafe { base.add(usize::from(last)) }.cast())
                            .unwrap();
                    usize::from(last) + object.size() == used
                }) && (used == 0) == offsets.is_empty(),
                "minor collection requires a complete TLAB header index"
            );
            for &offset in &offsets {
                let offset = usize::from(offset);
                self.stop_work.metadata_objects += 1;
                self.stop_work.metadata_bytes += GC_HEADER_SIZE as u64;
                // SAFETY: minor collection has already validated this retired prefix.
                let object = HeapObject::from_raw(unsafe { base.add(offset) }.cast())
                    .expect("TLAB header address is non-null");
                // Young objects left in a source chunk were either copied
                // (forwarded) or unreachable; reclaim both in this same walk.
                if source && object.allocated() && object.generation() == GC_GENERATION_YOUNG {
                    self.stop_work.swept_objects += 1;
                    let size = object.size();
                    let type_id = object.type_id();
                    let drop_fn = match last_type {
                        Some((cached, drop_fn)) if cached == type_id => drop_fn,
                        _ => {
                            let drop_fn = self.registry.drops.get(&type_id).copied();
                            last_type = Some((type_id, drop_fn));
                            drop_fn
                        }
                    };
                    let forwarded = object.forwarded_to().is_some();
                    if let Some(drop_fn) = drop_fn
                        && !forwarded
                    {
                        // SAFETY: unreachable young objects still own their runtime payload.
                        unsafe { super::run_drop_hook(drop_fn, object.payload().as_ptr()) };
                    }
                    if poison_moved && forwarded {
                        // A holder that kept the pre-move address instead of
                        // reloading its root slot now reads poison, not a
                        // plausible stale copy.
                        // SAFETY: the moved source payload is collector-owned.
                        unsafe {
                            std::ptr::write_bytes(
                                object.payload().as_ptr(),
                                MOVED_PAYLOAD_POISON,
                                size - GC_HEADER_SIZE,
                            )
                        };
                    }
                    object.reclaim_in_place();
                    reclaimed_young += 1;
                    self.state.allocated_bytes = self.state.allocated_bytes.saturating_sub(size);
                    self.state.young_allocated_bytes =
                        self.state.young_allocated_bytes.saturating_sub(size);
                    reclaimed_bytes = reclaimed_bytes.saturating_add(size);
                }
                if object.allocated() {
                    has_young |= object.generation() == GC_GENERATION_YOUNG;
                    debug_assert!(
                        object.generation() == GC_GENERATION_OLD
                            || self.state.tlab_chunks[chunk_index].kind == RegionKind::Survivor,
                        "young survivors must be in collector-owned destination storage"
                    );
                    has_allocated = true;
                    live_bytes = live_bytes.saturating_add(object.size());
                    self.state.tlab_chunks[chunk_index]
                        .mark_bitmap
                        .mark_stopped(offset);
                }
            }
            self.state.tlab_chunks[chunk_index].header_offsets = offsets;
            if !has_allocated {
                self.release_chunk(chunk_index, &mut chunk_identities);
            } else {
                if !has_young {
                    self.state.tlab_chunks[chunk_index].kind = RegionKind::Pinned;
                } else {
                    self.state.survivor_stats.survivor_space_reserved +=
                        self.state.tlab_chunks[chunk_index].capacity as u64;
                    self.state.survivor_stats.survivor_space_live += live_bytes as u64;
                }
                self.state.tlab_chunks[chunk_index].live_bytes = live_bytes;
                chunk_index += 1;
            }
        }
        // Every forwarded source is a distinct reclaimed young object; the
        // rest were unreachable and count as frees.
        self.state.total_frees = self
            .state
            .total_frees
            .saturating_add(reclaimed_young.saturating_sub(self.forwarded_objects as u64));
        for (index, &identity) in chunk_identities.iter().enumerate() {
            chunk_positions[identity] = Some(index);
        }
        self.state.tlab_addresses.remap(&chunk_positions);
        if let Some(cycle) = self.state.concurrent_cycle.clone() {
            cycle.objects.refresh_stopped(self.state);
            // Publication must scan promoted black objects too; enqueue() would
            // suppress already-black destinations allocated in existing old regions.
            let mut pending = Vec::with_capacity(32);
            let mut scanned_work = crate::gc_telemetry::MarkWork::default();
            let publication_cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
            for address in self.published.drain(..) {
                cycle.objects.claim(address);
                if self.already_queued.contains(&address) {
                    continue;
                }
                let object =
                    HeapObject::from_payload(GcPayload::from_raw(address as *mut u8).unwrap());
                let metadata = object.trace_metadata();
                if !self.registry.trace.contains_key(&metadata.type_id)
                    && cycle.account_scanned_inline(metadata, &mut scanned_work)
                {
                    continue;
                }
                pending.push(crate::gc_mark_queue::ObjectRef::from_addr(address).unwrap());
                if pending.len() == 32 {
                    cycle
                        .queue
                        .inject(crate::gc_mark_queue::MarkWork::new(
                            cycle.mark_epoch,
                            crate::gc_mark_queue::MarkWorkItem::ObjectBatch(
                                std::mem::take(&mut pending).into_boxed_slice(),
                            ),
                        ))
                        .expect("minor publications retain the old epoch");
                }
            }
            if !pending.is_empty() {
                cycle
                    .queue
                    .inject(crate::gc_mark_queue::MarkWork::new(
                        cycle.mark_epoch,
                        crate::gc_mark_queue::MarkWorkItem::ObjectBatch(pending.into_boxed_slice()),
                    ))
                    .expect("minor publications retain the old epoch");
            }
            for child in self.major_old_edges.drain() {
                cycle.enqueue(child as *mut u8);
            }
            // The collector already owns the epoch under STW. Account all
            // scanned publications once; no marker can race this minor phase.
            {
                let cpu = publication_cpu_start.and_then(|start| {
                    crate::gc_telemetry::workers::thread_cpu_ns()?.checked_sub(start)
                });
                let mut total = cycle.work.lock().unwrap();
                total.collector_mark_cpu_ns =
                    total.collector_mark_cpu_ns.saturating_add(cpu.unwrap_or(0));
                if let Some(cpu) = cpu.and_then(|cpu| total.cpu_ns.checked_add(cpu)) {
                    total.cpu_ns = cpu;
                } else {
                    total.cpu_incomplete = true;
                }
                total.frame_scans = total.frame_scans.saturating_add(scanned_work.frame_scans);
                total.objects = total.objects.saturating_add(scanned_work.objects);
                total.marked_bytes = total.marked_bytes.saturating_add(scanned_work.marked_bytes);
                total.scanned_bytes = total
                    .scanned_bytes
                    .saturating_add(scanned_work.scanned_bytes);
                total.descriptor_bytes = total
                    .descriptor_bytes
                    .saturating_add(scanned_work.descriptor_bytes);
            }
            cycle.termination.restart();
        }
        crate::gc_telemetry::milestones::reached(crate::gc_telemetry::GcStage::SweepComplete);
        crate::gc_telemetry::observations::graph_work(self.work);
        (reclaimed_bytes, std::mem::take(&mut self.work))
    }

    /// Whether an unindexed source chunk may hold an object with a drop hook.
    /// O(1): only its slow-path first object at offset 0 can, unless
    /// `generated_drop`.
    fn may_drop(&self, chunk_index: usize) -> bool {
        if self.registry.generated_drop {
            return true;
        }
        let chunk = &self.state.tlab_chunks[chunk_index];
        // SAFETY: the runtime slow path initialized the header at offset 0 of
        // this nonempty chunk before publishing it to generated code.
        let first = HeapObject::from_raw(chunk.base.cast()).expect("chunk base is non-null");
        self.registry.drops.contains_key(&first.type_id())
    }

    /// Debug-only check of the unwalked dead-chunk shortcut: its start bits
    /// tile the bump prefix with allocated young objects without drop hooks.
    #[cfg(debug_assertions)]
    fn verify_dead_chunk(&self, chunk_index: usize) {
        let chunk = &self.state.tlab_chunks[chunk_index];
        let mut expected = 0;
        chunk.mark_bitmap.bits.for_each_set(|index| {
            let offset = index * GC_REGION_MARK_GRANULE;
            assert_eq!(offset, expected, "generated TLAB header was not published");
            // SAFETY: `offset` ends the previous object inside the prefix.
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
            assert!(
                object.allocated()
                    && object.generation() == GC_GENERATION_YOUNG
                    && !self.registry.drops.contains_key(&object.type_id())
                    && object.forwarded_to().is_none(),
                "unwalked dead chunk holds a live, old or droppable object"
            );
            expected = offset + object.size();
        });
        assert_eq!(
            expected, chunk.used,
            "generated TLAB header was not published"
        );
    }

    /// Free a source chunk none of whose objects remain allocated.
    fn release_chunk(&mut self, chunk_index: usize, chunk_identities: &mut Vec<usize>) {
        let chunk = self.state.tlab_chunks.swap_remove(chunk_index);
        chunk_identities.swap_remove(chunk_index);
        let layout = Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
            .expect("TLAB chunk layout remains valid");
        // SAFETY: every object in this retired chunk was reclaimed or moved.
        unsafe { dealloc(chunk.base, layout) };
        self.state.released_bytes = self
            .state
            .released_bytes
            .saturating_add(chunk.capacity as u64);
        self.state.tlab_reserved_bytes = self
            .state
            .tlab_reserved_bytes
            .saturating_sub(chunk.capacity);
    }
}

/// Fill for the payload a minor collection moved away from, written by debug
/// runtimes and under `WILLOW_GC_STRESS=relocate`.
/// Pointer words become non-canonical addresses on every 64-bit target.
pub(super) const MOVED_PAYLOAD_POISON: u8 = 0xA5;

/// Roots of one minor collection. Slots are rewritten when their referent
/// moves; values come from owners parked inside runtime code, which cannot
/// observe a move. A young value defers the collection unless a hard memory
/// budget forces in-place retention.
#[derive(Default)]
pub(super) struct MinorRoots {
    pub(super) slots: Vec<*mut *mut u8>,
    pub(super) values: Vec<(*mut u8, PinSource)>,
}

/// Why a minor collection retained a young object in place. Reported by
/// `WILLOW_GC_VERIFY_NO_PIN`, which turns any young pin into a fatal error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PinSource {
    /// A root slot published outside a relocation-safe scope, where runtime
    /// Rust frames may hold raw copies of the referent. Reaches `pin_root`
    /// only under a hard memory budget or after the bounded deferral
    /// (`should_defer_minor`) runs out.
    RuntimeCodeStack,
    /// A remembered-set owner (always old in a consistent heap).
    RememberedOwner,
    /// A root slot naming an object that is neither a live young source nor a
    /// destination of this cycle.
    OldOrForeign,
    /// A hard memory budget left no evacuation space.
    MemoryBudget,
    /// An explicitly scoped interior/foreign address loan.
    ScopedLoan,
}

impl PinSource {
    fn describe(self) -> &'static str {
        match self {
            Self::RuntimeCodeStack => "root slot published from runtime code",
            Self::RememberedOwner => "remembered-set owner",
            Self::OldOrForeign => "root slot outside the young source chunks",
            Self::MemoryBudget => "memory budget left no evacuation space",
            Self::ScopedLoan => "scoped fixed-address loan",
        }
    }
}

/// Whether `WILLOW_GC_VERIFY_NO_PIN` is enabled: set to a non-empty value
/// other than `0`. Read once; consulted only when a young object is about to
/// be pinned.
fn verify_no_pin() -> bool {
    static VERIFY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        verify_no_pin_enabled(std::env::var_os("WILLOW_GC_VERIFY_NO_PIN").as_deref())
    });
    *VERIFY
}

fn verify_no_pin_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// Run one minor collection over `roots`, or return `None` to defer it.
///
/// Value roots come from threads parked inside runtime code, whose Rust frames
/// may keep raw copies of a young referent (willow-9tls.9). Such a park is
/// normally transient: the frame resumes after this stop and a later
/// allocation slow path retries, one TLAB chunk later (`minor_retry_bytes`).
/// Deferring instead of pinning keeps every nursery object movable. The
/// deferral is bounded: once young mutator allocation reaches the captured nursery
/// byte budget since the first pending request or root deferral, or under a hard memory
/// budget that cannot grow the nursery, the collection runs and promotes only
/// those runtime-held referents in place.
fn minor_collect_with_roots(
    mut roots: MinorRoots,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> Option<(u64, u64, crate::gc_telemetry::MarkWork)> {
    let root_scan =
        crate::gc_telemetry::measurement::Span::new(crate::gc_telemetry::measurement::Phase::Roots);
    append_parked_roots(&mut roots);
    roots.slots.extend(runtime_root_slots());
    roots.values.extend(
        runtime()
            .pin_lease_slots
            .snapshot()
            .into_iter()
            .filter_map(|slot| {
                RootSlot::from_raw(slot.cast())
                    .and_then(RootSlot::load)
                    .map(|value| (value.as_ptr(), PinSource::ScopedLoan))
            }),
    );
    // Address runtime roots are old. Their initial children are remembered at
    // registration and subsequent stores use the old-to-young barrier. Scanning
    // the complete registry here would revisit every suspended task each minor.
    drop(root_scan);
    let registry = minor_registry_snapshot();
    let mut state = runtime().heap.lock().unwrap();
    // Promotion raises `occupied` per object; drop the lock-free headrooms
    // once instead of charging each copy.
    super::invalidate_external_headroom();
    if should_defer_minor(&mut state, &roots) {
        state.survivor_stats.deferred_minor_collections += 1;
        let end = state
            .minor_deferral_start_bytes
            .unwrap()
            .saturating_add(state.minor_deferral_budget_bytes);
        state.minor_retry_bytes = state
            .young_allocation_clock
            .saturating_add(super::GC_TLAB_CHUNK_SIZE as u64)
            .min(end);
        return None;
    }
    state.minor_deferral_start_bytes = None;
    state.minor_deferral_budget_bytes = 0;
    state.survivor_stats.minor_blockers = 0;
    state.minor_retry_bytes = 0;
    if std::env::var("WILLOW_GC_VERIFY_BARRIER").is_ok()
        && let Err(message) = verify_remembered_set(&state, &registry.trace)
    {
        crate::gc_telemetry::failure_record::emit("invariant", &message);
        panic!("willow gc: write barrier verification failed: {message}");
    }
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    state.minor_collections = state.minor_collections.saturating_add(1);
    let before = state.allocated_bytes as u64;
    let young_before = state.young_allocated_bytes;
    let promoted_before = state.promoted_bytes;
    let copied_before = state.survivor_stats.survivor_bytes;
    let pinned_before = state.survivor_stats.pinned_promotions;
    let collector = MinorCollector::new(&mut state, registry, stop_work);
    let mut mark_roots = collector.snapshot_roots();
    roots
        .slots
        .extend(mark_roots.iter_mut().map(|p| p as *mut *mut u8));
    let (_, work) = collector.run(roots, remembered);
    if state.survivor_stats.pinned_promotions != pinned_before {
        state.survivor_stats.pin_fallback_count += 1;
    }
    let survived = state.promoted_bytes.saturating_sub(promoted_before)
        + state
            .survivor_stats
            .survivor_bytes
            .saturating_sub(copied_before);
    state.tenuring.complete(young_before, survived);
    state.nursery_threshold_bytes = state.nursery_policy.next(
        state.nursery_threshold_bytes,
        young_before,
        survived,
        state.memory_limit_bytes,
    );
    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(&state)
    {
        panic!("willow gc: region verification failed after minor collection: {message}");
    }
    let allocated_total = super::policy_allocated_total(&state);
    state
        .pacer
        .resume_allocation(crate::gc_telemetry::timestamp_ns(), allocated_total);
    super::progress::record_success(state.total_allocated_bytes);
    Some((before, state.allocated_bytes as u64, work))
}

/// Whether a runtime frame parked outside a relocation-safe scope holds a
/// young value root and the collection may still wait for it to resume. The
/// wait is bounded by one captured nursery threshold of young allocation,
/// measured from the first pending request or consecutive root deferral.
/// A pending request's captured budget is never restarted by the root scan.
/// Collection is requested at a refill boundary; concurrent active TLABs can
/// overshoot by their bump runway.
/// Reservation caps remain independently enforced by the allocation paths.
fn should_defer_minor(state: &mut super::GcState, roots: &MinorRoots) -> bool {
    #[cfg(test)]
    LEASE_POLICY_VISITS.set(0);
    // A scoped loan already requires in-place promotion. Its unsafe-stack
    // aliases cannot become movable by waiting, so do not accrue deferral debt
    // for them. Build the membership index once, rather than scanning leases
    // for every stack root (O(roots + loans), expected).
    let leased: AddressSet = roots
        .values
        .iter()
        .filter_map(|&(value, source)| {
            #[cfg(test)]
            LEASE_POLICY_VISITS.set(LEASE_POLICY_VISITS.get() + 1);
            (source == PinSource::ScopedLoan).then_some(value as usize)
        })
        .collect();
    let blockers = roots
        .values
        .iter()
        .filter(|&&(value, source)| {
            #[cfg(test)]
            LEASE_POLICY_VISITS.set(LEASE_POLICY_VISITS.get() + 1);
            source == PinSource::RuntimeCodeStack
                && !leased.contains(&(value as usize))
                && tlab_payload_generation(state, value as usize) == Some(GC_GENERATION_YOUNG)
        })
        .count();
    state.survivor_stats.minor_blockers = blockers as u64;
    if state.memory_limit_bytes.is_some() || force_pin_fallback() || blockers == 0 {
        return false;
    }
    if state.minor_deferral_start_bytes.is_none() {
        state.minor_deferral_start_bytes = Some(state.young_allocation_clock);
        state.minor_deferral_budget_bytes = state.nursery_threshold_bytes as u64;
    }
    let age = super::minor_deferral_age(state);
    state.young_allocation_clock != u64::MAX && age < state.minor_deferral_budget_bytes
}

#[cfg(test)]
thread_local! {
    static FORCE_PIN_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LEASE_POLICY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn force_pin_fallback() -> bool {
    FORCE_PIN_FALLBACK.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn force_pin_fallback() -> bool {
    false
}

#[cfg(test)]
pub(crate) fn deferred_minor_collections_for_test() -> u64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .survivor_stats
        .deferred_minor_collections
}

/// Run a minor collection that promotes young referents of runtime-held value
/// roots in place, as under a hard memory budget, instead of deferring. Unit
/// fixtures use it to drive the remaining pinned-region paths without a
/// budget (willow-9tls.9). The flag is thread-local so concurrently running
/// tests that expect deferral are unaffected.
#[cfg(test)]
pub(crate) fn minor_collect_pinning_for_test() {
    FORCE_PIN_FALLBACK.with(|force| force.set(true));
    minor_collect_internal();
    FORCE_PIN_FALLBACK.with(|force| force.set(false));
}

// Only the major owner may relocate while its epoch exists. Requests coalesce;
// initial/remark/sweep requests are serviced at a safe owner boundary.
static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn reset_pending() {
    PENDING.store(false, std::sync::atomic::Ordering::Release);
}

pub(super) fn requested() -> bool {
    PENDING.load(std::sync::atomic::Ordering::Acquire)
}

pub(super) fn service_pending() {
    if PENDING.swap(false, std::sync::atomic::Ordering::AcqRel) {
        collect_owned();
    }
}

fn collect_stopped(
    coord: &super::GcCoord,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> Option<(u64, u64, crate::gc_telemetry::MarkWork)> {
    {
        let mut state = runtime().heap.lock().unwrap();
        assert!(
            state.sweeping.is_none(),
            "minor relocation must wait for old sweep finalization"
        );
        if let Some(cycle) = &state.concurrent_cycle {
            assert_eq!(cycle.collector, std::thread::current().id());
        }
        super::flush_satb_all_locked(coord, &mut state);
        retire_all_tlabs_unindexed(&mut state);
    }
    let root_scan =
        crate::gc_telemetry::measurement::Span::new(crate::gc_telemetry::measurement::Phase::Roots);
    let roots = minor_stack_roots(coord);
    drop(root_scan);
    let result = minor_collect_with_roots(roots, stop_work);
    if let Some(cycle) = runtime().heap.lock().unwrap().concurrent_cycle.as_ref()
        && !cycle.closing.load(std::sync::atomic::Ordering::Acquire)
    {
        cycle.satb_shards.reopen_stopped(coord);
    }
    result
}

pub(super) fn collect_owned() {
    let cycle = crate::gc_telemetry::Cycle::begin(crate::gc_telemetry::CycleKind::Minor);
    let collected = with_stw(
        crate::gc_telemetry::stops::StopReason::Minor,
        |coord, stop_work| {
            if foreign_root_stack_owner_active_locked(coord) {
                super::progress::record_skip(super::GcSkipReason::ForeignOwnerAtHandshake);
                return None;
            }
            collect_stopped(coord, stop_work)
        },
    );
    if let Some((before, after, work)) = collected {
        crate::gc_telemetry::emit_cycle(cycle.finish(before, after, work));
    }
}

pub(crate) fn minor_collect_internal() {
    if runtime()
        .stop_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        PENDING.store(true, std::sync::atomic::Ordering::Release);
        willow_gc_safepoint();
        return;
    }
    let Some(_serialize) = super::cycle_wait::CollectorGuard::try_acquire() else {
        PENDING.store(true, std::sync::atomic::Ordering::Release);
        willow_gc_safepoint();
        return;
    };
    if foreign_root_stack_owner_active() {
        super::progress::record_skip(super::GcSkipReason::ForeignOwner);
        return;
    }

    // This elected minor satisfies any request left at a previous owner boundary.
    PENDING.swap(false, std::sync::atomic::Ordering::AcqRel);
    let cycle = crate::gc_telemetry::Cycle::begin(crate::gc_telemetry::CycleKind::Minor);
    // Stop the world and scan every registered mutator, for the reason spelled
    // out over the major cycle's mark phase: a registration that races a
    // single-mutator scan leaves the newcomer's objects unmarked (willow-v6k0).
    let collected = with_stw(
        crate::gc_telemetry::stops::StopReason::Minor,
        |coord, stop_work| {
            // Recheck under the stop's coord hold: an owner may have left the
            // registry, stack still nonempty, after the unlocked check above.
            if foreign_root_stack_owner_active_locked(coord) {
                return None;
            }
            Some(collect_stopped(coord, stop_work))
        },
    );
    let Some(collected) = collected else {
        super::progress::record_skip(super::GcSkipReason::ForeignOwnerAtHandshake);
        return;
    };
    let Some((before, after, work)) = collected else {
        return;
    };
    let event = cycle.finish(before, after, work);
    drop(_serialize);
    crate::gc_telemetry::emit_cycle(event);
}
