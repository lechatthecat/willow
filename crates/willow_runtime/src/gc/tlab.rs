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

pub(super) fn read_tlab_delta(record: &mut TlabStateRecord) -> (u64, u64) {
    #[cfg(test)]
    TLAB_ACCOUNTING_RECORD_VISITS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: records are removed before their owner's generated TLS expires.
    let tls = unsafe { tlab_state_at(record.address) };
    let allocations = tls.fast_allocations.load(Ordering::Acquire);
    let bytes = tls.fast_allocated_bytes.load(Ordering::Acquire);
    let delta = (
        allocations.saturating_sub(record.observed_fast_allocations),
        bytes.saturating_sub(record.observed_fast_allocated_bytes),
    );
    record.observed_fast_allocations = allocations;
    record.observed_fast_allocated_bytes = bytes;
    delta
}
pub(super) fn add_tlab_accounting(state: &mut GcState, allocations: u64, bytes: u64) {
    state.total_allocs = state.total_allocs.saturating_add(allocations);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
    state.tlab_fast_allocations = state.tlab_fast_allocations.saturating_add(allocations);
    state.tlab_fast_allocated_bytes = state.tlab_fast_allocated_bytes.saturating_add(bytes);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.young_allocated_bytes = state.young_allocated_bytes.saturating_add(bytes as usize);
}
pub(super) fn sync_tlab_accounting(state: &mut GcState) {
    let (mut allocations, mut bytes) = (0u64, 0u64);
    for record in state.tlab_states.values_mut() {
        let delta = read_tlab_delta(record);
        allocations = allocations.saturating_add(delta.0);
        bytes = bytes.saturating_add(delta.1);
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
            assist_observed_fast_bytes: 0,
            observed_fast_allocations: 0,
            observed_fast_allocated_bytes: 0,
        }
    });
}

pub(super) fn retire_tlab_locked(state: &mut GcState, address: usize) -> usize {
    let Some(record) = state.tlab_states.get_mut(&address) else {
        return 0;
    };
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
        chunk.owner_state = None;
        assert!(
            chunk.header_offsets.is_empty(),
            "TLAB indexed more than once"
        );
        // The owner is stopped or retiring its own chunk; generated headers
        // are now immutable except for collector liveness/generation fields.
        let mut offset = 0;
        while offset < chunk.used {
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
            let size = object.size();
            assert!(
                size >= GC_HEADER_SIZE
                    && size <= chunk.used - offset
                    && size.is_multiple_of(GC_REGION_MARK_GRANULE),
                "corrupt retired TLAB header"
            );
            assert!(
                chunk.mark_bitmap.is_marked(offset),
                "generated TLAB header was not published"
            );
            chunk
                .header_offsets
                .push(u16::try_from(offset).expect("TLAB offset fits bounded chunk"));
            offset += size;
        }
        return chunk.header_offsets.len();
    }
    0
}

pub(super) fn retire_all_tlabs_locked(state: &mut GcState) -> usize {
    sync_tlab_accounting(state);
    let addresses: Vec<usize> = state.tlab_states.keys().copied().collect();
    let mut headers = 0;
    for address in addresses {
        headers += retire_tlab_locked(state, address);
    }
    headers
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
        let record = state
            .tlab_states
            .get_mut(&address)
            .expect("owner index has a TLS record");
        debug_assert_eq!(record.owner, owner);
        let (allocations, bytes) = read_tlab_delta(record);
        // Retirement at the initial root handshake starts this owner's epoch
        // after all pre-snapshot allocation. Do not bill it to the new cycle.
        record.assist_observed_fast_bytes = record.observed_fast_allocated_bytes;
        add_tlab_accounting(state, allocations, bytes);
        retire_tlab_locked(state, address);
        if unregister {
            state.tlab_states.remove(&address);
        }
    }
    if unregister {
        state.tlab_owners.remove(&owner);
    }
}

pub(super) fn retire_tlabs_for_thread(owner: ThreadId) {
    debug_assert_eq!(owner, std::thread::current().id());
    if !HAS_REGISTERED_TLAB.replace(false) {
        return;
    }
    retire_owned_tlabs_locked(&mut runtime().heap.lock().unwrap(), owner, true);
}

pub(super) fn allocate_tlab_chunk(state: &mut GcState, owner_state: usize) -> Option<*mut u8> {
    if !can_reserve(state, GC_TLAB_CHUNK_SIZE) {
        return None;
    }
    let layout =
        Layout::from_size_align(GC_TLAB_CHUNK_SIZE, std::mem::align_of::<GcHeader>()).ok()?;
    // SAFETY: the layout is nonzero and valid. Fresh zeroing makes every
    // unallocated payload byte safe before generated code publishes a header.
    let base = unsafe { allocate_region_storage(layout) };
    if base.is_null() {
        return None;
    }
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
        mark_bitmap: RegionMarkBitmap::new(GC_TLAB_CHUNK_SIZE),
        concurrent_marks: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
            GC_TLAB_CHUNK_SIZE / GC_REGION_MARK_GRANULE,
        )),
        header_offsets: Vec::new(),
    });
    let chunk = state.tlab_chunks.last().unwrap();
    // SAFETY: the generated TLS is owned by this allocation's mutator.
    unsafe { tlab_state_at(owner_state) }.start_bits.store(
        chunk.mark_bitmap.bits.words_ptr() as usize,
        Ordering::Release,
    );
    state.tlab_reserved_bytes = state.tlab_reserved_bytes.saturating_add(GC_TLAB_CHUNK_SIZE);
    state.tlab_refills = state.tlab_refills.saturating_add(1);
    state
        .tlab_states
        .get_mut(&owner_state)
        .expect("TLAB state is registered before refill")
        .current_chunk = Some(base as usize);
    Some(base)
}
