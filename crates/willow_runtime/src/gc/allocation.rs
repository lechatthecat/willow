use super::*;

/// # Safety
/// A non-null address must be exclusive header-aligned space for the complete header and
/// payload. The type, payload extent and tracing mask must agree, including any required
/// bitmap descriptor or native hooks. Initialize native fields before the object can be
/// traced or finalized.
pub(super) unsafe fn initialize_object_at(
    header: *mut u8,
    total_size: usize,
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
) -> Option<HeapObject> {
    // SAFETY: The allocation cursor reserves this aligned header and its complete payload.
    // The supplied type and mask describe the initialized words.
    unsafe {
        HeapObject::initialize_at(
            header,
            total_size,
            type_id,
            layout_id,
            gc_ref_mask,
            GC_GENERATION_YOUNG,
        )
    }
}

#[cfg(test)]
/// Reservation occupancy can remain high even when few objects are live. A
/// level-triggered check would repeatedly scan the same rooted region. Keep a
/// pressure latch across cycles; normal occupancy/pacer triggers and reservation
/// admission still operate independently while the latch is set.
pub(super) fn hard_reservation_pressure(state: &mut GcState) -> bool {
    hard_reservation_pressure_in(runtime(), state)
}

pub(super) fn hard_reservation_pressure_in(owner: &GcRuntime, state: &mut GcState) -> bool {
    let Some(limit) = state.memory_limit_bytes else {
        state.hard_pressure_triggered = false;
        return false;
    };
    let reserved = state
        .old_reserved_bytes
        .saturating_add(state.tlab_reserved_bytes)
        .saturating_add(owner.native_accounting.bytes.load(Ordering::Relaxed));
    if reserved <= limit / 2 {
        state.hard_pressure_triggered = false;
    }
    if reserved >= (limit / 4).saturating_mul(3).max(1) && !state.hard_pressure_triggered {
        state.hard_pressure_triggered = true;
        return true;
    }
    false
}

/// Mirror of `concurrent_cycle.is_some() || sweeping.is_some()`, written under
/// the heap lock by every change to either, so allocation can skip a policy
/// check that would only report "busy" without contending on the heap lock. A stale read only
/// shifts the check to the next slow path.
pub(super) fn publish_collection_busy(state: &GcState) {
    publish_collection_busy_in(runtime(), state);
}

pub(super) fn publish_collection_busy_in(owner: &GcRuntime, state: &GcState) {
    owner.collection_busy.store(
        state.concurrent_cycle.is_some() || state.sweeping.is_some(),
        Ordering::Release,
    );
}

// TLAB refills already publish their own bytes; other live buffers may lag by
// at most their current chunk. Telemetry and non-TLAB callers retain full sync.
pub(super) fn allocation_should_collect_with_sync_in(
    owner: &crate::runtime::Runtime,
    sync: bool,
) -> bool {
    let stress = owner.diagnostics.stress_enabled("alloc");
    if !stress && owner.gc.collection_busy.load(Ordering::Acquire) {
        return false;
    }
    let mut state = owner.gc.heap.lock().unwrap();
    allocation_policy_due_in(owner, &mut state, sync, stress)
}

pub(super) fn allocation_policy_due_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    sync: bool,
    stress: bool,
) -> bool {
    if !stress && (state.concurrent_cycle.is_some() || state.sweeping.is_some()) {
        return false;
    }
    if sync {
        tlab::sync_tlab_bytes_in(&owner.gc, state);
    }
    let bytes = policy_allocated_total_in(&owner.gc, state);
    if state.pacer.sample_due(bytes) {
        state
            .pacer
            .allocation(crate::gc_telemetry::timestamp_ns(), bytes);
        pacer::sample_threshold_in(owner, state);
    }
    let hard_pressure = hard_reservation_pressure_in(&owner.gc, state);
    state.soft_memory.sample_process();
    let memory_input = memory_inputs_in(&owner.gc, state);
    let memory = state.soft_memory.decide(memory_input);
    // The pacer trigger lives in the major-policy domain even when a soft
    // limit makes the memory inputs count every young byte.
    let paced = state.pacer.enabled()
        && major_policy_managed_bytes(state)
            .saturating_add(owner.gc.native_accounting.bytes.load(Ordering::Relaxed))
            as u64
            >= state.pacer_trigger
        && memory.reason != memory_control::Reason::Relief;
    let collect = stress || hard_pressure || paced || memory.collect;
    if !collect {
        owner
            .gc
            .headroom
            .policy
            .store(policy_headroom_in(&owner.gc, state), Ordering::Release);
    }
    collect
}

pub(super) fn minor_stress_enabled_in(owner: &crate::runtime::Runtime) -> bool {
    owner.diagnostics.stress_enabled("minor") || owner.diagnostics.stress_enabled("relocate")
}

pub(super) fn allocation_should_minor_collect_in(owner: &crate::runtime::Runtime) -> bool {
    let stress = minor_stress_enabled_in(owner);
    let state = owner.gc.heap.lock().unwrap();
    stress || minor_allocation_due(&state)
}

/// Debt retries remain enabled even when a major sweep lowers occupancy.
pub(super) fn minor_allocation_due(state: &GcState) -> bool {
    if state.minor_deferral_start_bytes.is_some() {
        state.memory_limit_bytes.is_some()
            || state.young_allocation_clock >= state.minor_retry_bytes
    } else {
        state.young_allocated_bytes >= state.nursery_threshold_bytes
    }
}

pub(super) fn minor_deferral_age(state: &GcState) -> u64 {
    state.minor_deferral_start_bytes.map_or(0, |start| {
        debug_assert!(
            state.young_allocation_clock >= start,
            "young allocation clock regressed"
        );
        state.young_allocation_clock - start
    })
}

#[cfg(test)]
/// A queued minor may use one fixed nursery budget before joining its owner.
/// Start the clock at the request, not at the eventual root scan: otherwise
/// repeated refills during a major sweep would have unbounded, uncharged debt.
pub(super) fn minor_refill_should_wait(state: &mut GcState) -> bool {
    minor_refill_should_wait_in(runtime(), state)
}

pub(super) fn minor_refill_should_wait_in(owner: &GcRuntime, state: &mut GcState) -> bool {
    if !minor_allocation_due(state) {
        return false;
    }
    if state.minor_deferral_start_bytes.is_none()
        && minor::requested(owner)
        && state.memory_limit_bytes.is_none()
        // Do not grant an unassessed refill whose full runway exceeds the budget.
        && state.nursery_threshold_bytes >= GC_TLAB_CHUNK_SIZE
        && state.young_allocation_clock != u64::MAX
    {
        state.minor_deferral_start_bytes = Some(state.young_allocation_clock);
        state.minor_deferral_budget_bytes = state.nursery_threshold_bytes as u64;
        state.minor_retry_bytes = state
            .young_allocation_clock
            .saturating_add(GC_TLAB_CHUNK_SIZE as u64);
    }
    state.minor_deferral_start_bytes.is_none()
        || state.young_allocation_clock == u64::MAX
        || minor_deferral_age(state) >= state.minor_deferral_budget_bytes
}

// ---------------------------------------------------------------------------
// Public runtime API
// ---------------------------------------------------------------------------

/// Set the soft memory target in bytes (managed pressure and best-effort
/// process RSS); zero disables it. Returns the previous limit. Collection is considered at the next allocation slow path;
/// this setter never parks mutators or changes the legacy hard reservation cap.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_set_memory_limit(bytes: u64) -> u64 {
    let mut state = runtime().heap.lock().unwrap();
    let previous = state.soft_memory.set_limit(bytes);
    state.soft_memory.sample_process();
    evict_region_cache_for(&mut state, 0);
    drop(state);
    invalidate_runtime_policy_budgets();
    previous
}

/// Initialize the GC runtime.
///
/// Production code calls this once at process startup, before any allocation.
/// Calling it again resets the single process-global heap and invalidates
/// existing GC pointers, so it is not a general-purpose runtime reset API.
/// Unit tests may intentionally reset the heap of the runtime their thread
/// serves; `isolated_runtime()` gives each test thread its own runtime.
///
/// # Safety
/// The caller must have exclusive lifecycle access to this runtime. Stop other mutators and
/// exclude allocations, initializers, borrows and all other heap users throughout reset.
/// Release outstanding roots and leases or otherwise guarantee no stale pointers, slots or
/// handles are later used. Registered TLAB storage must still be live during retirement.
///
/// ```compile_fail,E0133
/// willow_runtime::gc::willow_gc_init();
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_init() {
    // SAFETY: The caller has quiesced this runtime and relinquished access to its old heap.
    unsafe { reset_internal() };
    // Generated array fast paths can allocate a reference buffer before any
    // runtime array call. Re-register after every heap/registry reset.
    crate::array::ensure_trace_registered();
    crate::runtime::current()
        .diagnostic_ready
        .store(true, Ordering::Release);
}

/// Allocate a GC-managed object of `payload_size` bytes with the given
/// `type_id`.  Returns a pointer to the **payload** (past the header), or
/// null on allocation failure.
///
/// This function may trigger a collection if the heap threshold is exceeded.
///
/// # Safety
/// The effective u32 type ID must admit this payload size and a zero inline mask. The bitmap
/// type is invalid here because this API has no descriptor argument. For a native type,
/// register matching hooks and initialize the complete native representation before any
/// safepoint, publication, or finalization.
///
/// ```compile_fail,E0133
/// use willow_runtime::gc::willow_alloc_object;
/// willow_alloc_object(i64::from(willow_abi::GC_BITMAP_TYPE_ID), 8);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_alloc_object(type_id: i64, payload_size: i64) -> *mut u8 {
    // SAFETY: The caller supplies the matching payload layout and initialization protocol for
    // the effective type ID.
    unsafe { allocate_object(0, type_id as u32, payload_size, 0) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_alloc_typed(payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    // SAFETY: Type zero is the ordinary zero-initialized payload. Its inline mask is bounded
    // by the payload word count during tracing.
    unsafe { allocate_object(0, 0, payload_size, gc_ref_mask) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_alloc(payload_size: i64) -> *mut u8 {
    willow_alloc_typed(payload_size, 0)
}

/// Compatibility layout-aware allocation ABI used by runtime-owned values.
///
/// Generated Willow code uses its inlined TLS bump path and calls
/// `willow_gc_alloc_slow` only on refill/large/stress paths. Runtime containers
/// without access to the generated TLS block use the old-region slow path.
/// Allocate an object with scalable tracing metadata. `descriptor` points to
/// immutable, aligned static u64 data `[count, bits...]`, alive until all such
/// objects have been reclaimed. Payload and bitmap sizes must agree.
/// `layout_id` is the address-independent fingerprint computed by the compiler.
///
/// # Safety
/// Null is rejected before any read. For a non-null descriptor,
/// the aligned immutable bitmap must contain the count followed by that many u64 tracing
/// words covering the payload, with exactly its reference slots set. Keep it live for all GC
/// uses and supply the matching layout fingerprint.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_alloc_bitmap(
    layout_id: i64,
    payload_size: i64,
    descriptor: *const u64,
) -> *mut u8 {
    assert!(!descriptor.is_null());
    assert!(payload_size >= 0 && payload_size % GC_STORAGE_WORD_BYTES as i64 == 0);
    // SAFETY: The bitmap ABI supplies an immutable u64-aligned descriptor beginning with its word
    // count.
    let count = unsafe { *descriptor } as usize;
    assert_eq!(
        count,
        (payload_size as usize / GC_STORAGE_WORD_BYTES).div_ceil(64)
    );
    // SAFETY: The count matches the payload width and the caller retains the aligned
    // descriptor through all tracing.
    unsafe {
        allocate_object(
            layout_id as u64,
            willow_abi::GC_BITMAP_TYPE_ID,
            payload_size,
            descriptor as u64,
        )
    }
}

/// # Safety
/// The effective u32 type ID, payload size, alignment, tracing metadata and layout ID must
/// describe the same object. For the bitmap type the mask is the address of an aligned
/// immutable [count, bits...] descriptor covering all payload words, live through reclamation
/// and any collector snapshots. For native types, install compatible hooks and initialize
/// their full payload before any safepoint, publication, or possible finalization. Maintain
/// initialized managed references in traced slots.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_alloc_layout(
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    // SAFETY: The caller supplies the matching payload layout and initialization protocol for
    // the effective type ID.
    unsafe { allocate_object(layout_id, type_id as u32, payload_size, gc_ref_mask) }
}

/// Allocation slow path for compiler-generated TLAB lowering.
///
/// `tlab_state` is the current thread's generated TLS block. Small allocations
/// retire an exhausted chunk, coordinate collection/threshold checks, refill,
/// initialize the first object, and return its payload. Large and stress-mode
/// allocations stay on the old/large-region path.
///
/// # Safety
/// A non-null TLAB must be the initialized, naturally aligned allocation block of this mutator
/// in the selected runtime (initially zeroed). Keep its address and storage valid until
/// retirement AND removal from the registry, including during stops and teardown; only the
/// owning mutator may bump it, following the collector stop protocol. The effective u32 type
/// ID, payload size, alignment, tracing metadata and layout ID must describe the same object.
/// For the bitmap type the mask is the address of an aligned immutable [count, bits...]
/// descriptor covering all payload words, live through reclamation and any collector
/// snapshots. For native types, install compatible hooks and initialize their full payload
/// before any safepoint, publication, or possible finalization. Maintain initialized managed
/// references in traced slots.
///
/// ```compile_fail,E0133
/// use willow_runtime::gc::{willow_gc_alloc_slow, willow_gc_current_tlab};
/// willow_gc_alloc_slow(willow_gc_current_tlab(), 0, 0, 8, 0);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_alloc_slow(
    tlab_state: *mut GcTlabState,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    let _relocation = RelocationSafeScope::enter();
    if tlab_state.is_null() || payload_size < 0 {
        crate::failure::fatal_invariant("invalid TLAB allocation arguments");
    }
    // SAFETY: The caller retains this owner-thread TLAB through unregistration and supplies
    // the matching type and tracing layout.
    unsafe {
        tlab_alloc_slow_in(
            crate::runtime::current(),
            tlab_state as usize,
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
        )
    }
}

/// Shared TLAB miss handling for generated code and the runtime's own TLAB
/// ([`runtime_tlab`]). The caller decides whether this thread's root slots are
/// relocatable for any collection started here.
///
/// # Safety
/// A non-null TLAB must be the initialized, naturally aligned allocation block of this mutator
/// in the selected runtime (initially zeroed). Keep its address and storage valid until
/// retirement AND removal from the registry, including during stops and teardown; only the
/// owning mutator may bump it, following the collector stop protocol. The effective u32 type
/// ID, payload size, alignment, tracing metadata and layout ID must describe the same object.
/// For the bitmap type the mask is the address of an aligned immutable [count, bits...]
/// descriptor covering all payload words, live through reclamation and any collector
/// snapshots. For native types, install compatible hooks and initialize their full payload
/// before any safepoint, publication, or possible finalization. Maintain initialized managed
/// references in traced slots.
pub(super) unsafe fn tlab_alloc_slow(
    state_address: usize,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    // SAFETY: The caller retains this owner-thread TLAB through unregistration and supplies
    // the matching type and tracing layout.
    unsafe {
        tlab_alloc_slow_in(
            crate::runtime::current(),
            state_address,
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
        )
    }
}

/// # Safety
/// A non-null TLAB must be the initialized, naturally aligned allocation block of this mutator
/// in the selected runtime (initially zeroed). Keep its address and storage valid until
/// retirement AND removal from the registry, including during stops and teardown; only the
/// owning mutator may bump it, following the collector stop protocol. The effective u32 type
/// ID, payload size, alignment, tracing metadata and layout ID must describe the same object.
/// For the bitmap type the mask is the address of an aligned immutable [count, bits...]
/// descriptor covering all payload words, live through reclamation and any collector
/// snapshots. For native types, install compatible hooks and initialize their full payload
/// before any safepoint, publication, or possible finalization. Maintain initialized managed
/// references in traced slots.
pub(super) unsafe fn tlab_alloc_slow_in(
    owner: &crate::runtime::Runtime,
    state_address: usize,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    let Some(total_size) = (GC_HEADER_SIZE)
        .checked_add(payload_size as usize)
        .and_then(|size| size.checked_next_multiple_of(std::mem::align_of::<GcHeader>()))
    else {
        crate::failure::resource_exhausted(format_args!("TLAB allocation size overflow"));
    };
    let stress = owner.diagnostics.stress_enabled("alloc");
    let small = total_size <= GC_TLAB_MAX_OBJECT_SIZE;
    let (fast_bytes, minor_due, cached, major_due, storage_allocator) = {
        let mut state = owner.gc.heap.lock().unwrap();
        // SAFETY: The mutator owns this initialized block at a stable address through
        // retirement and removal from this runtime.
        unsafe { tlab::register_tlab_state_in(&owner.gc, &mut state, state_address) };
        let record = state.tlab_states.get_mut(&state_address).unwrap();
        let fast = fast_bytes_total(record);
        let fast_bytes = fast.saturating_sub(record.assist_observed_fast_bytes);
        record.assist_observed_fast_bytes = fast;
        tlab::sync_tlab_record_bytes_in(&owner.gc, &mut state, state_address);
        if small {
            // A small-object miss means the active chunk has insufficient tail
            // space. Seal it before collection/refill.
            tlab::retire_tlab_locked_in(&owner.telemetry.footprint, &mut state, state_address);
        }
        // The refilling record is current; other owners publish at their own
        // refill, bounding unobserved nursery bytes by one chunk per TLAB.
        let cached = if small && !stress {
            state.region_cache.take_survivor()
        } else {
            None
        };
        let minor_due = minor_allocation_due(&state);
        let major_due = if !stress && !minor_due && !minor_stress_enabled_in(owner) {
            Some(allocation_policy_due_in(owner, &mut state, false, false))
        } else {
            None
        };
        (
            fast_bytes,
            minor_due,
            cached,
            major_due,
            state.storage_allocator.share(),
        )
    };
    let cached_storage = cached.map(tlab::ChunkStorage::from_cached);
    sweep::assist_in(owner);
    marking::assist_concurrent_mark_in(owner, fast_bytes.saturating_add(total_size as u64));

    if !stress
        && (minor_due || minor_stress_enabled_in(owner))
        && allocation_should_minor_collect_in(owner)
    {
        minor::minor_collect_in(owner);
        // A busy owner may only have queued the request. Account for its debt
        // immediately; join only when no bounded refill runway remains.
        while small
            && {
                let mut state = owner.gc.heap.lock().unwrap();
                minor_refill_should_wait_in(&owner.gc, &mut state)
            }
            && cycle_wait::wait_for_minor_budget_in(owner)
        {
            // The owner can satisfy the request before publishing completion.
            // Recheck rather than start another minor for every waiting refill.
            if allocation_should_minor_collect_in(owner) {
                minor::minor_collect_in(owner);
            }
        }
    }

    if stress {
        let _ = try_collect_internal_in(owner);
    } else if major_due.unwrap_or_else(|| allocation_should_collect_with_sync_in(owner, false)) {
        automatic_collect_in(owner, false);
    }

    if stress || !small {
        // SAFETY: The allocation path forwards the validated payload extent, type and tracing
        // layout; its caller initializes native fields before collection.
        return unsafe {
            allocate_old_in(owner, layout_id, type_id as u32, payload_size, gc_ref_mask)
        };
    }

    let mut storage = cached_storage.or_else(|| tlab::ChunkStorage::allocate(&storage_allocator));
    let mut state = owner.gc.heap.lock().unwrap();
    // SAFETY: The mutator owns this initialized block at a stable address through retirement
    // and removal from this runtime.
    unsafe { tlab::register_tlab_state_in(&owner.gc, &mut state, state_address) };
    let mut base = tlab::install_tlab_chunk(owner, &mut state, state_address, &mut storage);
    for _ in 0..BUDGET_COLLECTION_RETRIES {
        if base.is_some() || (state.memory_limit_bytes.is_none() && !state.soft_memory.enabled()) {
            break;
        }
        let released = state.released_bytes;
        drop(state);
        collect_for_budget_in(owner);
        if storage.is_none() {
            storage = tlab::ChunkStorage::allocate(&storage_allocator);
        }
        state = owner.gc.heap.lock().unwrap();
        // SAFETY: The mutator owns this initialized block at a stable address through
        // retirement and removal from this runtime.
        unsafe { tlab::register_tlab_state_in(&owner.gc, &mut state, state_address) };
        base = tlab::install_tlab_chunk(owner, &mut state, state_address, &mut storage);
        if state.released_bytes == released {
            break;
        }
    }
    let Some(base) = base else {
        return allocation_failure_in(&owner.gc, &state);
    };
    let Some(header) =
        // SAFETY: The allocation cursor reserves this aligned header and its complete
        // payload. The supplied type and mask describe the initialized words.
        (unsafe { initialize_object_at(base, total_size, type_id as u32, layout_id, gc_ref_mask) })
    else {
        crate::failure::fatal_invariant("fresh TLAB could not initialize a validated object");
    };
    state.tlab_chunks.last_mut().unwrap().mark_bitmap.mark(0);
    // SAFETY: the generated TLS block is aligned and remains alive for this
    // thread. Publish limit before cursor; generated code resumes only after
    // this slow-path call returns.
    let tls = unsafe { tlab_state_at(state_address) };
    // Relocation stress closes the TLAB after one object, so every generated
    // allocation returns here and runs a moving minor collection first.
    let limit = if owner.diagnostics.stress_enabled("relocate") {
        total_size
    } else {
        GC_TLAB_CHUNK_SIZE
    };
    tls.limit.store(base as usize + limit, Ordering::Release);
    tls.cursor
        .store(base as usize + total_size, Ordering::Release);
    // Fast-path bytes in this chunk are measured from here.
    state
        .tlab_states
        .get_mut(&state_address)
        .expect("TLAB state is registered before refill")
        .chunk_fast_start = base as usize + total_size;
    owner.gc.headroom.consume(total_size);
    state.allocated_bytes = state.allocated_bytes.saturating_add(total_size);
    state.young_allocated_bytes = state.young_allocated_bytes.saturating_add(total_size);
    state.young_allocation_clock = state
        .young_allocation_clock
        .saturating_add(total_size as u64);
    state.total_allocs = state.total_allocs.saturating_add(1);
    state.total_allocated_bytes = state
        .total_allocated_bytes
        .saturating_add(total_size as u64);
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(1);
    header.payload().as_ptr()
}

pub(super) fn chunk_used_bytes(state: &GcState, chunk: &BumpChunk) -> usize {
    let start = chunk.base as usize;
    chunk
        .owner_state
        .and_then(|address| state.tlab_states.get(&address))
        .map(|record| {
            // SAFETY: an active chunk's registered TLS state remains valid.
            let cursor = unsafe { tlab_state_at(record.address) }
                .cursor
                .load(Ordering::Acquire);
            cursor
                .clamp(start, start.saturating_add(chunk.capacity))
                .saturating_sub(start)
        })
        .unwrap_or(chunk.used)
}

pub(super) fn object_in_retired_chunk(
    chunk: &BumpChunk,
    address: usize,
    interior: bool,
) -> Option<HeapObject> {
    let relative = address.checked_sub(chunk.base as usize + GC_HEADER_SIZE)?;
    let offset = if chunk.needs_index() {
        // Not indexed yet: its start bits name exactly its
        // published headers. O(chunk words) for an interior lookup.
        if interior {
            chunk.mark_bitmap.last_start_in(0, relative)?
        } else {
            (relative < chunk.used
                && relative.is_multiple_of(GC_REGION_MARK_GRANULE)
                && chunk.mark_bitmap.is_marked(relative))
            .then_some(relative)?
        }
    } else {
        indexed_header_offset(chunk, relative, interior)?
    };
    // SAFETY: a validated header index or a published start bit names a
    // header; chunk storage cannot be released while the caller holds the
    // heap mutex.
    let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
    if !object.allocated() {
        return None;
    }
    let payload = object.payload().as_ptr() as usize;
    ((!interior && payload == address)
        || (interior && address >= payload && address < object.as_ptr() as usize + object.size()))
    .then_some(object)
}

pub(super) fn indexed_header_offset(
    chunk: &BumpChunk,
    relative: usize,
    interior: bool,
) -> Option<usize> {
    let index = if interior {
        chunk
            .header_offsets
            .partition_point(|&offset| {
                chunk.lookup_comparisons.observe(|count| {
                    count.fetch_add(1, Ordering::Relaxed);
                });
                usize::from(offset) <= relative
            })
            .checked_sub(1)?
    } else {
        chunk
            .header_offsets
            .binary_search_by(|offset| {
                chunk.lookup_comparisons.observe(|count| {
                    count.fetch_add(1, Ordering::Relaxed);
                });
                usize::from(*offset).cmp(&relative)
            })
            .ok()?
    };
    Some(usize::from(chunk.header_offsets[index]))
}

pub(super) fn old_region_objects(state: &GcState) -> impl Iterator<Item = HeapObject> + '_ {
    state.old_regions.iter().flat_map(|region| {
        region.allocations.keys().map(|&offset| {
            // SAFETY: the region allocation map owns the header at this offset.
            HeapObject::from_raw(unsafe { region.base.add(offset) }.cast()).unwrap()
        })
    })
}

pub(super) fn find_old_region_object(
    state: &GcState,
    address: usize,
    interior: bool,
) -> Option<HeapObject> {
    let index = state
        .old_addresses
        .candidate(address.checked_sub(GC_HEADER_SIZE)?)?;
    state.old_regions[index].object_for_address(address, interior)
}

pub(super) fn find_tlab_chunk(state: &GcState, address: usize) -> Option<&BumpChunk> {
    let chunk = &state.tlab_chunks[state
        .tlab_addresses
        .candidate(address.checked_sub(GC_HEADER_SIZE)?)?];
    let start = chunk.base as usize;
    let end = start.saturating_add(chunk_used_bytes(state, chunk));
    (address >= start + GC_HEADER_SIZE && address <= end).then_some(chunk)
}

/// # Safety
/// The effective u32 type ID, payload size, alignment, tracing metadata and layout ID must
/// describe the same object. For the bitmap type the mask is the address of an aligned
/// immutable [count, bits...] descriptor covering all payload words, live through reclamation
/// and any collector snapshots. For native types, install compatible hooks and initialize
/// their full payload before any safepoint, publication, or possible finalization. Maintain
/// initialized managed references in traced slots.
pub(super) unsafe fn allocate_object(
    layout_id: u64,
    type_id: u32,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    if payload_size < 0 {
        crate::failure::fatal_invariant("negative managed allocation size");
    }
    let total_size = (payload_size as u64).saturating_add(GC_HEADER_SIZE as u64);
    let owner = crate::runtime::current();
    sweep::assist_in(owner);
    assist_concurrent_mark_in(owner, total_size);
    let stress = owner.diagnostics.stress_enabled("alloc");
    // A due check charges the chunk it covers against the lock-free headroom
    // first; only an exhausted headroom consults the policy under the lock.
    if (stress
        || (runtime_policy_check_due_in(&owner.gc, total_size)
            && !consume_policy_headroom(&owner.gc, GC_TLAB_CHUNK_SIZE as u64 + total_size)))
        && allocation_should_collect_with_sync_in(owner, true)
    {
        automatic_collect(stress);
    }
    // SAFETY: The allocation path forwards the validated payload extent, type and tracing
    // layout; its caller initializes native fields before collection.
    unsafe { allocate_old(layout_id, type_id, payload_size, gc_ref_mask) }
}

// Process-wide identity allocation prevents a reused owner address or equal
// epochs from accepting another collector's cached allocation budget.
pub(super) static NEXT_POLICY_IDENTITY: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_policy_identity() -> u64 {
    NEXT_POLICY_IDENTITY
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("collector policy identities exhausted")
}

std::thread_local! {
    /// Last collector identity, epoch and bytes since the policy check.
    /// This cache contains no owner pointer and cannot prolong its lifetime.
    pub(super) static RUNTIME_POLICY_DEBT: std::cell::Cell<(u64, u64, u64)> = const { std::cell::Cell::new((0, 0, 0)) };
}

pub(super) fn invalidate_runtime_policy_budgets() {
    invalidate_runtime_policy_budgets_in(runtime());
}

pub(super) fn invalidate_runtime_policy_budgets_in(owner: &GcRuntime) {
    owner.policy_epoch.fetch_add(1, Ordering::AcqRel);
    owner.headroom.invalidate();
}

/// Record growth of reserved heap storage. Under a hard reservation cap the
/// pressure trigger depends on reservations rather than allocated bytes, so a
/// reservation re-arms every thread's next policy check and the trigger fires
/// at the same allocation as without the per-thread budget. Reservations grow
/// once per region or chunk, so this costs one check per thread per growth.
#[cfg(test)]
pub(super) fn note_reservation_growth(state: &GcState) {
    note_reservation_growth_in(crate::runtime::current(), state);
}

pub(super) fn note_reservation_growth_in(owner: &crate::runtime::Runtime, state: &GcState) {
    owner.telemetry.footprint.reservation(
        (state.old_reserved_bytes as u64).saturating_add(state.tlab_reserved_bytes as u64),
    );
    if state.memory_limit_bytes.is_some() {
        invalidate_runtime_policy_budgets_in(&owner.gc);
    }
}

/// Whether this runtime allocation of `bytes` should consult the collection
/// policy. Generated code decides only at TLAB refills, once per
/// `GC_TLAB_CHUNK_SIZE` bytes per thread; runtime containers use the same
/// per-thread granularity instead of locking the heap for every string or
/// container allocation. A thread's first allocation, and the first after a
/// policy-input change or hard-capped reservation growth, always checks.
/// Reservation limits stay exact because `allocate_old` enforces them under
/// the heap lock; pacing and soft-limit triggers may fire up to one chunk of
/// runtime allocation per thread later, as for generated code.
pub(super) fn runtime_policy_check_due_in(owner: &GcRuntime, bytes: u64) -> bool {
    let epoch = owner.policy_epoch.load(Ordering::Acquire);
    RUNTIME_POLICY_DEBT.with(|debt| {
        let (identity, seen, owed) = debt.get();
        let owed = if identity == owner.policy_identity && seen == epoch {
            owed.saturating_add(bytes)
        } else {
            u64::MAX
        };
        let due = owed >= GC_TLAB_CHUNK_SIZE as u64;
        debt.set((owner.policy_identity, epoch, if due { 0 } else { owed }));
        due
    })
}

/// Add a newly reserved regular region if it still has usable space.
pub(super) fn index_old_region(state: &mut GcState, index: usize) {
    let region = &state.old_regions[index];
    if region.kind == RegionKind::Old && !region.sweep_quarantined {
        let available = region.available_span();
        if available >= GC_HEADER_SIZE {
            state.old_region_candidates.insert((available, index));
        }
    }
}

#[cfg(test)]
/// # Safety
/// The type, tracing metadata and payload size must satisfy the allocation contract of
/// willow_gc_alloc_layout, including reserved bitmap descriptors and native initialization.
pub(super) unsafe fn allocate_old_region_object_locked(
    state: &mut GcState,
    layout_id: u64,
    type_id: u32,
    payload_size: usize,
    gc_ref_mask: u64,
    count_logical_allocation: bool,
) -> Option<HeapObject> {
    // SAFETY: The allocation path forwards the validated payload extent, type and tracing
    // layout; its caller initializes native fields before collection.
    unsafe {
        allocate_old_region_object_locked_in(
            crate::runtime::current(),
            state,
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
            count_logical_allocation,
        )
    }
}

/// # Safety
/// The type, tracing metadata and payload size must satisfy the allocation contract of
/// willow_gc_alloc_layout, including reserved bitmap descriptors and native initialization.
pub(super) unsafe fn allocate_old_region_object_locked_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    layout_id: u64,
    type_id: u32,
    payload_size: usize,
    gc_ref_mask: u64,
    count_logical_allocation: bool,
) -> Option<HeapObject> {
    allocate_old_region_object_for_move_in(
        owner,
        state,
        raw_heap::TraceMetadata {
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
        },
        count_logical_allocation,
        None,
    )
}

pub(super) fn allocate_old_region_object_for_move_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    metadata: raw_heap::TraceMetadata,
    count_logical_allocation: bool,
    source: Option<HeapObject>,
) -> Option<HeapObject> {
    let payload_size = metadata.payload_size;
    let total_size = GC_HEADER_SIZE.checked_add(payload_size)?;
    let span_size = total_size.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?;
    let large = total_size > GC_LARGE_OBJECT_THRESHOLD;

    while state
        .old_region_candidates
        .last()
        .is_some_and(|&(_, index)| state.old_regions[index].sweep_quarantined)
    {
        // Each stale candidate is discarded at most once during the sweep.
        state.old_region_candidates.pop_last();
    }
    let candidate = state
        .old_region_candidates
        .last()
        .copied()
        .filter(|(available, _)| !large && *available >= span_size);
    let (object, reused) = if let Some((_, index)) = candidate {
        // SAFETY: The caller establishes the type and layout and exclusive access to any
        // evacuation source. The region owns the destination storage.
        let allocated = unsafe {
            state.old_regions[index].allocate_object_for_move_buffered(
                owner.gc.mark_phase(),
                metadata,
                source,
                None,
            )
        }?;
        let available = state.old_regions[index].available_span();
        state.old_region_candidates.pop_last();
        if available >= GC_HEADER_SIZE {
            state.old_region_candidates.insert((available, index));
        }
        allocated
    } else {
        let capacity = if large { span_size } else { GC_OLD_REGION_SIZE };
        let kind = if large {
            RegionKind::LargeObject
        } else {
            RegionKind::Old
        };
        let mut region = reserve_old_region_in(&owner.gc, state, kind, capacity)?;
        // SAFETY: The caller establishes the type and layout and exclusive access to any
        // evacuation source. The region owns the destination storage.
        let allocated = unsafe {
            region.allocate_object_for_move_buffered(owner.gc.mark_phase(), metadata, source, None)
        }?;
        state
            .old_addresses
            .insert(region.base as usize, state.old_regions.len());
        state.old_regions.push(region);
        state.old_reserved_bytes += capacity;
        note_reservation_growth_in(owner, state);
        index_old_region(state, state.old_regions.len() - 1);
        allocated
    };

    account_old_region_allocation_in(
        &owner.gc,
        state,
        object.size(),
        reused,
        count_logical_allocation,
    );
    Some(object)
}

#[inline]
pub(super) fn account_old_region_allocation_in(
    owner: &GcRuntime,
    state: &mut GcState,
    size: usize,
    reused: bool,
    count_logical_allocation: bool,
) {
    owner.headroom.consume(size);
    state.allocated_bytes = state.allocated_bytes.saturating_add(size);
    state.old_region_allocations = state.old_region_allocations.saturating_add(1);
    if reused {
        state.old_region_reuses = state.old_region_reuses.saturating_add(1);
    }
    if count_logical_allocation {
        state.total_allocs = state.total_allocs.saturating_add(1);
        state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(size as u64);
    }
}

/// # Safety
/// The type, tracing metadata and payload size must satisfy the allocation contract of
/// willow_gc_alloc_layout, including reserved bitmap descriptors and native initialization.
pub(super) unsafe fn allocate_old(
    layout_id: u64,
    type_id: u32,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    // SAFETY: The allocation path forwards the validated payload extent, type and tracing
    // layout; its caller initializes native fields before collection.
    unsafe {
        allocate_old_in(
            crate::runtime::current(),
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
        )
    }
}

/// # Safety
/// The type, tracing metadata and payload size must satisfy the allocation contract of
/// willow_gc_alloc_layout, including reserved bitmap descriptors and native initialization.
pub(super) unsafe fn allocate_old_in(
    owner: &crate::runtime::Runtime,
    layout_id: u64,
    type_id: u32,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    RuntimePathObservation::old_allocation(|| &owner.gc.path_observation);
    if payload_size < 0 {
        crate::failure::fatal_invariant("negative managed allocation size");
    }
    let payload_size = payload_size as usize;
    // Large runtime allocations can consume an arbitrary number of bytes
    // between generated loop polls. Service the initial handshake too, when
    // no mark epoch exists yet and mark assists therefore return immediately.
    if payload_size > GC_TLAB_MAX_OBJECT_SIZE.saturating_sub(GC_HEADER_SIZE)
        && !cycle_wait::is_collector()
    {
        safepoint::safepoint_in(owner);
    }
    if old_lab::enabled_in(owner)
        && let Some((total, span)) = old_lab::span_for(payload_size)
        && total <= GC_TLAB_MAX_OBJECT_SIZE
        && old_lab::available_in(owner)
    {
        // Small runtime objects bump-allocate in this thread's old buffer and
        // take the heap mutex only to refill it.
        if let Some(object) =
            // SAFETY: The allocation path forwards the validated payload extent, type and
            // tracing layout; its caller initializes native fields before collection.
            unsafe {
                old_lab::try_allocate_in(owner, type_id, layout_id, gc_ref_mask, total, span)
            }
        {
            return object.payload().as_ptr();
        }
        let mut state = owner.gc.heap.lock().unwrap();
        let refill = |state: &mut GcState| {
            // SAFETY: The allocation path forwards the validated payload extent, type and
            // tracing layout; its caller initializes native fields before collection.
            unsafe {
                old_lab::refill_and_allocate_in(
                    owner,
                    state,
                    type_id,
                    layout_id,
                    gc_ref_mask,
                    total,
                    span,
                )
            }
        };
        let mut object = refill(&mut state);
        for _ in 0..BUDGET_COLLECTION_RETRIES {
            if object.is_some()
                || (state.memory_limit_bytes.is_none() && !state.soft_memory.enabled())
            {
                break;
            }
            let released = state.released_bytes;
            drop(state);
            collect_for_budget_in(owner);
            state = owner.gc.heap.lock().unwrap();
            object = refill(&mut state);
            if state.released_bytes == released {
                break;
            }
        }
        let Some(object) = object else {
            return allocation_failure_in(&owner.gc, &state);
        };
        return object.payload().as_ptr();
    }
    // TLAB byte merging is O(TLABs); the policy checks and TLAB slow path that
    // read those bytes merge them, so the per-object path does not.
    let mut state = owner.gc.heap.lock().unwrap();
    // A coordinator request can still be queued before collector election.
    // Large allocations must not turn OS scheduling latency into unbounded
    // heap growth. Request/join initial publication once the fixed request
    // runway is spent, then use Retry-aware assists: marking may depend on
    // a lock held by this allocator even if no epoch exists yet.
    if state.concurrent_cycle.is_none()
        && state.sweeping.is_none()
        && major_policy_managed_bytes(&state)
            > state
                .threshold_bytes
                .saturating_add(sweep::ALLOCATION_RUNWAY as usize)
        && !cycle_wait::is_collector()
    {
        drop(state);
        cycle_wait::start_for_allocation_in(owner);
        state = owner.gc.heap.lock().unwrap();
    }
    // New objects are outside the sweep snapshot. Bound this floating garbage
    // rather than feeding unlimited sweep-time growth into the next live goal.
    // Check under the allocation lock, so concurrent allocators share one
    // budget and overshoot by at most one admitted object.
    while sweep::allocation_budget_exhausted(&state) {
        drop(state);
        if !cycle_wait::wait_for_sweep_budget_in(owner) {
            state = owner.gc.heap.lock().unwrap();
            break;
        }
        state = owner.gc.heap.lock().unwrap();
    }
    // SAFETY: The allocation path forwards the validated payload extent, type and tracing
    // layout; its caller initializes native fields before collection.
    let mut header = unsafe {
        allocate_old_region_object_locked_in(
            owner,
            &mut state,
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
            true,
        )
    };
    for _ in 0..BUDGET_COLLECTION_RETRIES {
        if header.is_some() || (state.memory_limit_bytes.is_none() && !state.soft_memory.enabled())
        {
            break;
        }
        let released = state.released_bytes;
        drop(state);
        collect_for_budget_in(owner);
        state = owner.gc.heap.lock().unwrap();
        // SAFETY: The allocation path forwards the validated payload extent, type and tracing
        // layout; its caller initializes native fields before collection.
        header = unsafe {
            allocate_old_region_object_locked_in(
                owner,
                &mut state,
                layout_id,
                type_id,
                payload_size,
                gc_ref_mask,
                true,
            )
        };
        if state.released_bytes == released {
            break;
        }
    }
    let Some(header) = header else {
        return allocation_failure_in(&owner.gc, &state);
    };
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(1);
    if header.size() > GC_TLAB_MAX_OBJECT_SIZE {
        state.tlab_large_allocations = state.tlab_large_allocations.saturating_add(1);
    }
    header.payload().as_ptr()
}
