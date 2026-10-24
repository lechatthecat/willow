use super::*;

// ---------------------------------------------------------------------------
// TLAB state and chunk management
// ---------------------------------------------------------------------------

pub(super) unsafe fn tlab_state_at(address: usize) -> &'static GcTlabState {
    // SAFETY: generated code passes the address of its aligned, zero-initialized
    // TLS block whose layout is locked by the compiler/runtime ABI tests.
    unsafe { &*(address as *const GcTlabState) }
}

#[cfg(test)]
pub(super) static TLAB_ACCOUNTING_RECORD_VISITS: AtomicUsize = AtomicUsize::new(0);

/// Fast-path bytes this TLAB has allocated so far: retired chunks plus the
/// active chunk's bump distance past its slow-path first object. O(1).
pub(super) fn fast_bytes_total(record: &TlabStateRecord) -> u64 {
    let active = if record.current_chunk.is_some() {
        // SAFETY: records are removed before their owner's generated TLS expires.
        let cursor = unsafe { tlab_state_at(record.address) }
            .cursor
            .load(Ordering::Acquire);
        cursor.saturating_sub(record.chunk_fast_start) as u64
    } else {
        0
    };
    record.retired_fast_bytes.saturating_add(active)
}

/// Fast-path objects this TLAB has allocated so far. The active chunk's start
/// words hold one bit per published header, including the slow-path first
/// object. O(chunk words), a constant 64 for the fixed chunk size.
fn fast_allocations_total(
    record: &TlabStateRecord,
    chunks: &[BumpChunk],
    addresses: &address_index::AddressIndex,
) -> u64 {
    let active = record
        .current_chunk
        .and_then(|base| addresses.exact(base))
        .map_or(0, |index| {
            (chunks[index].mark_bitmap.bits.count() as u64).saturating_sub(1)
        });
    record.retired_fast_allocations.saturating_add(active)
}

/// Bytes-only delta for allocation-path pacing; never scans start words.
fn read_tlab_bytes_delta(record: &mut TlabStateRecord) -> u64 {
    #[cfg(test)]
    TLAB_ACCOUNTING_RECORD_VISITS.fetch_add(1, Ordering::Relaxed);
    let bytes = fast_bytes_total(record);
    let delta = bytes.saturating_sub(record.observed_fast_allocated_bytes);
    record.observed_fast_allocated_bytes = bytes;
    delta
}

fn read_tlab_allocations_delta(
    record: &mut TlabStateRecord,
    chunks: &[BumpChunk],
    addresses: &address_index::AddressIndex,
) -> u64 {
    let allocations = fast_allocations_total(record, chunks, addresses);
    let delta = allocations.saturating_sub(record.observed_fast_allocations);
    record.observed_fast_allocations = allocations;
    delta
}

pub(super) fn add_tlab_accounting(state: &mut GcState, allocations: u64, bytes: u64) {
    state.total_allocs = state.total_allocs.saturating_add(allocations);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
    state.young_allocation_clock = state.young_allocation_clock.saturating_add(bytes);
    state.tlab_fast_allocations = state.tlab_fast_allocations.saturating_add(allocations);
    state.tlab_fast_allocated_bytes = state.tlab_fast_allocated_bytes.saturating_add(bytes);
    consume_headrooms(bytes as usize);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.young_allocated_bytes = state.young_allocated_bytes.saturating_add(bytes as usize);
}

/// Merge fast-path bytes for pacing and nursery triggers: O(TLABs), one cursor
/// load each. Object counts lag until [`sync_tlab_accounting`] or retirement.
pub(super) fn sync_tlab_bytes(state: &mut GcState) {
    let mut bytes = 0u64;
    for record in state.tlab_states.values_mut() {
        bytes = bytes.saturating_add(read_tlab_bytes_delta(record));
    }
    add_tlab_accounting(state, 0, bytes);
    old_lab::sync_bytes(state);
}

/// Merge fast-path bytes and object counts, for telemetry readers.
pub(super) fn sync_tlab_accounting(state: &mut GcState) {
    sync_tlab_counters(state);
    // Index consumers retain the baseline full flush.
    old_lab::flush_all(state);
}

pub(super) fn sync_tlab_accounting_unindexed(state: &mut GcState) {
    sync_tlab_counters(state);
    old_lab::sync_records(state);
}

fn sync_tlab_counters(state: &mut GcState) {
    let (mut allocations, mut bytes) = (0u64, 0u64);
    let GcState {
        tlab_states,
        tlab_chunks,
        tlab_addresses,
        ..
    } = state;
    for record in tlab_states.values_mut() {
        bytes = bytes.saturating_add(read_tlab_bytes_delta(record));
        allocations = allocations.saturating_add(read_tlab_allocations_delta(
            record,
            tlab_chunks,
            tlab_addresses,
        ));
    }
    add_tlab_accounting(state, allocations, bytes);
}

thread_local! {
    // Marker workers normally own no generated TLAB. Their unregister path
    // must not rescan every mutator's allocation counters/records each cycle.
    pub(super) static HAS_REGISTERED_TLAB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn register_tlab_state(state: &mut GcState, address: usize) {
    HAS_REGISTERED_TLAB.set(true);
    state.tlab_states.entry(address).or_insert_with(|| {
        state
            .tlab_owners
            .entry(std::thread::current().id())
            .or_default()
            .insert(address);
        TlabStateRecord {
            address,
            owner: std::thread::current().id(),
            current_chunk: None,
            chunk_fast_start: 0,
            retired_fast_allocations: 0,
            retired_fast_bytes: 0,
            assist_observed_fast_bytes: 0,
            observed_fast_allocations: 0,
            observed_fast_allocated_bytes: 0,
        }
    });
}

pub(super) fn retire_tlab_locked(state: &mut GcState, address: usize) {
    let Some(record) = state.tlab_states.get_mut(&address) else {
        return;
    };
    // Fold the active chunk into the retired totals so the derived counts stay
    // continuous across retirement.
    record.retired_fast_bytes = fast_bytes_total(record);
    // SAFETY: the record owns this generated TLS state until unregister/reset.
    let tls = unsafe { tlab_state_at(record.address) };
    let cursor = tls.cursor.swap(0, Ordering::AcqRel);
    tls.limit.store(0, Ordering::Release);
    tls.start_bits.store(0, Ordering::Release);
    let current_chunk = record.current_chunk.take();
    if let Some(base) = current_chunk
        && let Some(index) = state.tlab_addresses.exact(base)
    {
        let chunk = &mut state.tlab_chunks[index];
        let start = chunk.base as usize;
        let end = start.saturating_add(chunk.capacity);
        chunk.used = cursor.clamp(start, end).saturating_sub(start);
        crate::gc_telemetry::footprint::retired_tail((chunk.capacity - chunk.used) as u64);
        chunk.owner_state = None;
        assert!(
            chunk.header_offsets.is_empty(),
            "TLAB indexed more than once"
        );
        // Header indexing is deferred to the first walk that needs it
        // (`index_retired_chunk`), so a nursery chunk that dies entirely in
        // the next minor collection is never walked (willow-8hq4.21). Every
        // start bit but the slow-path first object came from the fast path.
        record.retired_fast_allocations = record
            .retired_fast_allocations
            .saturating_add((chunk.mark_bitmap.bits.count() as u64).saturating_sub(1));
    }
}

/// Build and validate the header index of a retired chunk that has none yet.
/// Returns the number of headers indexed: 0 for an indexed or empty chunk.
/// O(objects in the chunk), at most once per chunk.
///
/// The owner is stopped or retired; generated headers are immutable except
/// for collector liveness/generation fields. Header addresses come from the
/// published start bits rather than from chasing `offset += size`: each header
/// load is then independent of the previous one, and the size chain is only
/// compared, never used as an address (willow-8hq4.16). An unpublished header
/// shows up as a gap and an extra bit as an overlap.
pub(super) fn index_retired_chunk(chunk: &mut BumpChunk) -> usize {
    if !chunk.needs_index() {
        return 0;
    }
    let used = chunk.used;
    let mut expected = 0;
    let mut offsets = std::mem::take(&mut chunk.header_offsets);
    chunk.mark_bitmap.bits.for_each_set(|index| {
        let offset = index * GC_REGION_MARK_GRANULE;
        assert!(
            offset == expected && offset < used,
            "generated TLAB header was not published"
        );
        // SAFETY: `offset` is inside the retired prefix and equals the
        // end of the previous validated object, so it names a header.
        let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
        let size = object.size();
        assert!(
            size >= GC_HEADER_SIZE
                && size <= used - offset
                && size.is_multiple_of(GC_REGION_MARK_GRANULE),
            "corrupt retired TLAB header"
        );
        offsets.push(u16::try_from(offset).expect("TLAB offset fits bounded chunk"));
        expected = offset + size;
    });
    assert!(expected == used, "generated TLAB header was not published");
    chunk.header_offsets = offsets;
    chunk.header_offsets.len()
}

/// Seal every active TLAB without indexing its headers. The minor collector
/// indexes only the chunks it touches; other callers use
/// `retire_all_tlabs_locked`.
pub(super) fn retire_all_tlabs_unindexed(state: &mut GcState) {
    sync_tlab_accounting(state);
    retire_all_tlab_cursors(state);
}

/// Initial mark captures start maps and must not index old-LAB objects while
/// retiring nursery cursors. Minor and baseline index consumers keep their flush.
pub(super) fn retire_all_tlabs_unindexed_for_mark(state: &mut GcState) {
    // Retirement keeps the derived totals continuous, so one sync afterwards
    // also publishes the fast allocations folded in from retired chunks.
    retire_all_tlab_cursors(state);
    sync_tlab_accounting_unindexed(state);
}

fn retire_all_tlab_cursors(state: &mut GcState) {
    let addresses: Vec<usize> = state.tlab_states.keys().copied().collect();
    for address in addresses {
        retire_tlab_locked(state, address);
    }
}

/// Seal every active TLAB and index every retired chunk that still lacks its
/// header index. Returns the headers indexed. O(chunks + newly indexed headers).
pub(super) fn retire_all_tlabs_locked(state: &mut GcState) -> usize {
    retire_all_tlabs_unindexed(state);
    state.tlab_chunks.iter_mut().map(index_retired_chunk).sum()
}

pub(super) fn retire_tlabs_with_work(
    state: &mut GcState,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
) {
    let headers = retire_all_tlabs_locked(state) as u64;
    work.metadata_objects += headers;
    work.metadata_bytes += headers * GC_HEADER_SIZE as u64;
}

pub(super) fn retire_owned_tlabs_locked(state: &mut GcState, owner: ThreadId, unregister: bool) {
    let addresses: Vec<_> = state
        .tlab_owners
        .get(&owner)
        .into_iter()
        .flat_map(|addresses| addresses.iter().copied())
        .collect();
    for address in addresses {
        retire_tlab_record_locked(state, address, unregister);
    }
    if unregister {
        state.tlab_owners.remove(&owner);
    }
}

/// Retire one registered TLS block, folding its counters into the totals, and
/// optionally forget the record. The caller fixes up `tlab_owners`.
fn retire_tlab_record_locked(state: &mut GcState, address: usize, unregister: bool) {
    let record = state
        .tlab_states
        .get_mut(&address)
        .expect("owner index has a TLS record");
    let bytes = read_tlab_bytes_delta(record);
    // Retirement at the initial root handshake starts this owner's epoch
    // after all pre-snapshot allocation. Do not bill it to the new cycle.
    record.assist_observed_fast_bytes = record.observed_fast_allocated_bytes;
    add_tlab_accounting(state, 0, bytes);
    retire_tlab_locked(state, address);
    // With no active chunk this merges only the retired count: O(1).
    let GcState {
        tlab_states,
        tlab_chunks,
        tlab_addresses,
        ..
    } = &mut *state;
    let record = tlab_states.get_mut(&address).expect("retired record");
    let allocations = read_tlab_allocations_delta(record, tlab_chunks, tlab_addresses);
    add_tlab_accounting(state, allocations, 0);
    if unregister {
        state.tlab_states.remove(&address);
    }
}

/// Retire and forget the TLS block at `address` if it is registered, e.g.
/// before the block's thread-local storage is destroyed. O(1) expected.
pub(super) fn unregister_tlab_state(address: usize) {
    // Same coord -> heap order as mutator unregistration, so a collector never
    // observes a half-retired record.
    let (lock, _) = &runtime().coord;
    let _coord = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = runtime()
        .heap
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(owner) = state.tlab_states.get(&address).map(|record| record.owner) else {
        return;
    };
    retire_tlab_record_locked(&mut state, address, true);
    if let Some(addresses) = state.tlab_owners.get_mut(&owner) {
        addresses.remove(&address);
        if addresses.is_empty() {
            state.tlab_owners.remove(&owner);
        }
    }
}

pub(super) fn retire_tlabs_for_thread(owner: ThreadId) {
    debug_assert_eq!(owner, std::thread::current().id());
    if !HAS_REGISTERED_TLAB.replace(false) {
        return;
    }
    retire_owned_tlabs_locked(&mut runtime().heap.lock().unwrap(), owner, true);
}

/// Zeroed storage and start bitmaps for one TLAB chunk, prepared before the
/// refill takes the heap lock. Zeroing 32 KiB and allocating both bitmaps
/// inside the critical section serialized every concurrent refill
/// (willow-jz15.120). Dropping an uninstalled value releases the storage.
pub(super) struct ChunkStorage {
    block: StorageBlock,
    mark_bitmap: RegionMarkBitmap,
    concurrent_marks: Arc<concurrent_bitmap::ConcurrentMarkBits>,
}

struct StorageBlock(*mut u8);

impl StorageBlock {
    fn layout() -> Layout {
        Layout::from_size_align(GC_TLAB_CHUNK_SIZE, std::mem::align_of::<GcHeader>())
            .expect("TLAB chunk layout is valid")
    }

    /// Hand the storage to a chunk, whose cleanup releases it.
    fn into_raw(self) -> *mut u8 {
        let base = self.0;
        std::mem::forget(self);
        base
    }
}

impl Drop for StorageBlock {
    fn drop(&mut self) {
        // SAFETY: an uninstalled block still owns its storage exclusively.
        unsafe { dealloc(self.0, Self::layout()) };
    }
}

impl ChunkStorage {
    /// `None` when the system allocator fails; no heap lock is needed.
    pub(super) fn allocate() -> Option<Self> {
        // SAFETY: the layout is nonzero and valid. Fresh zeroing makes every
        // unallocated payload byte safe before generated code publishes a header.
        let base = unsafe { allocate_region_storage(StorageBlock::layout()) };
        (!base.is_null()).then(|| Self {
            block: StorageBlock(base),
            mark_bitmap: RegionMarkBitmap::new(GC_TLAB_CHUNK_SIZE),
            concurrent_marks: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                GC_TLAB_CHUNK_SIZE / GC_REGION_MARK_GRANULE,
            )),
        })
    }
}

#[cfg(test)]
pub(super) fn allocate_tlab_chunk(state: &mut GcState, owner_state: usize) -> Option<*mut u8> {
    install_tlab_chunk(state, owner_state, &mut ChunkStorage::allocate())
}

/// Install `storage` as `owner_state`'s current chunk, taking it only when
/// the reservation admits it. O(1) expected under the heap lock.
pub(super) fn install_tlab_chunk(
    state: &mut GcState,
    owner_state: usize,
    storage: &mut Option<ChunkStorage>,
) -> Option<*mut u8> {
    if storage.is_none() || !can_reserve(state, GC_TLAB_CHUNK_SIZE) {
        return None;
    }
    let ChunkStorage {
        block,
        mark_bitmap,
        concurrent_marks,
    } = storage.take().unwrap();
    let base = block.into_raw();
    runtime().tlab_ever_allocated.store(true, Ordering::Release);
    state
        .tlab_addresses
        .insert(base as usize, state.tlab_chunks.len());
    state.tlab_chunks.push(BumpChunk {
        base,
        capacity: GC_TLAB_CHUNK_SIZE,
        used: 0,
        owner_state: Some(owner_state),
        kind: RegionKind::Nursery,
        live_bytes: 0,
        mark_bitmap,
        concurrent_marks,
        header_offsets: Vec::new(),
    });
    let chunk = state.tlab_chunks.last().unwrap();
    // SAFETY: the generated TLS is owned by this allocation's mutator.
    unsafe { tlab_state_at(owner_state) }.start_bits.store(
        chunk.mark_bitmap.bits.words_ptr() as usize,
        Ordering::Release,
    );
    state.tlab_reserved_bytes = state.tlab_reserved_bytes.saturating_add(GC_TLAB_CHUNK_SIZE);
    note_reservation_growth(state);
    state.tlab_refills = state.tlab_refills.saturating_add(1);
    state
        .tlab_states
        .get_mut(&owner_state)
        .expect("TLAB state is registered before refill")
        .current_chunk = Some(base as usize);
    Some(base)
}
