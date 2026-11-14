//! Per-thread old-generation allocation buffers for runtime-side objects. A thread carves a
//! span of up to `OLD_LAB_BYTES` from an
//! old region under the heap mutex, then bump-allocates small objects in it
//! holding only its own uncontended buffer mutex. Each object is published
//! exactly as `OldRegion::allocate_object` publishes one: zeroed span,
//! initialized OLD header, black concurrent mark while marking, start bit, and
//! `begin_trace` while its region awaits a sweep visit. Region object indexing
//! (`allocations`, `live_bytes`) is deferred to a batched flush under the heap
//! mutex; until then lookups find the object through its start bit.
//!
//! Lock order is heap mutex, then buffer mutex. The owner never holds its
//! buffer mutex while acquiring the heap mutex.
//!
//! Invariants:
//! - A buffer owns a stable region base. Incremental sweep may swap vector
//!   positions; flush/retirement resolve that base through old_addresses before
//!   using the cached region index. New buffers survive unrelated releases.
//! - While marking is active, concurrent region publication may precede a
//!   buffer's cached sweep flag: allocation still sets the concurrent mark bit.
//!   Final mark closure retires these buffers before phase 0. Outside marking,
//!   the copy equals its region's flag; sweep retires buffers before visiting.
//! - Mark closure retires every buffer before clearing `GC_MARK_PHASE`, so no
//!   buffer allocation observes phase 0 while its region still awaits a
//!   pending sweep without `sweep_pending`.
//! - Every pending object is flushed into its region before a walk of
//!   `allocations` (sweep, baseline stopped marking, validation, reset).
//!   Concurrent epoch capture uses immutable headers/start maps and counter
//!   merging; it must not eagerly build per-object indexes under initial STW.
use super::*;

/// Carve size: four generated TLAB chunks (256 KiB, one old region). Runtime-heavy
/// pipelines refill while the sweeper holds the heap mutex region by region,
/// so a quarter of the refill rate measurably cuts blocking (GC allocation fast paths
/// audit); an idle buffer pins at most this much per thread until retirement.
pub(super) const OLD_LAB_BYTES: usize = 4 * GC_TLAB_CHUNK_SIZE;
/// Smallest carve preferred from an existing region. Smaller holes stay for
/// exact-fit promotion and large runtime allocations.
const OLD_LAB_MIN_BYTES: usize = 1024;

/// Selects buffer allocation without a test-only process setting. Production
/// uses the constant policy and does not evaluate the owner lookup.
pub(super) trait AllocationPolicy {
    fn enabled<'a>(_owner: impl FnOnce() -> &'a Self) -> bool
    where
        Self: 'a,
    {
        true
    }
}
impl AllocationPolicy for () {}
#[cfg(not(test))]
pub(super) type RuntimeAllocationPolicy = ();
#[cfg(test)]
pub(super) type RuntimeAllocationPolicy = std::sync::atomic::AtomicBool;
#[cfg(test)]
impl AllocationPolicy for RuntimeAllocationPolicy {
    fn enabled<'a>(owner: impl FnOnce() -> &'a Self) -> bool {
        owner().load(Ordering::Acquire)
    }
}

pub(super) fn enabled() -> bool {
    enabled_in(crate::runtime::current())
}

pub(super) fn enabled_in(owner: &crate::runtime::Runtime) -> bool {
    RuntimeAllocationPolicy::enabled(|| &owner.gc.old_lab_policy)
}

pub(super) struct OldLab {
    inner: Mutex<Inner>,
    /// Monotonic logical bytes/objects; written only under `inner`, read
    /// without it by the allocation-policy byte merge.
    bytes: AtomicU64,
    objects: AtomicU64,
}

struct Inner {
    active: bool,
    region: usize,
    base: usize,
    cursor: usize,
    limit: usize,
    reused: bool,
    sweep_pending: bool,
    starts: Option<Arc<concurrent_bitmap::ConcurrentMarkBits>>,
    marks: Option<Arc<concurrent_bitmap::ConcurrentMarkBits>>,
    /// Published `(offset, span)` entries of the current carve, ascending;
    /// `pending_index` separates the indexed prefix from the pending suffix.
    pending: Vec<(usize, usize)>,
    /// Already-indexed prefix retained in place across bounded remark retries.
    /// No prefix removal/copy and no repeated region insertion.
    pending_index: usize,
    pending_live: usize,
}

/// Heap-mutex-owned bookkeeping for one registered buffer.
pub(super) struct OldLabRecord {
    lab: Arc<OldLab>,
    observed_bytes: u64,
    observed_objects: u64,
}

impl OldLab {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                active: false,
                region: 0,
                base: 0,
                cursor: 0,
                limit: 0,
                reused: false,
                sweep_pending: false,
                starts: None,
                marks: None,
                pending: Vec::new(),
                pending_index: 0,
                pending_live: 0,
            }),
            bytes: AtomicU64::new(0),
            objects: AtomicU64::new(0),
        }
    }
}

thread_local! {
    static LOCAL: Arc<OldLab> = Arc::new(OldLab::new());
    // `LOCAL` belongs to the runtime this thread serves; collectors this
    // thread only calls into explicitly keep separate labs by identity.
    static PRIVATE: std::cell::RefCell<IdMap<u64, Arc<OldLab>>> =
        std::cell::RefCell::new(IdMap::default());
}

fn with_local_in<R>(gc: &GcRuntime, f: impl FnOnce(&Arc<OldLab>) -> R) -> Option<R> {
    if gc.serves_current_thread() {
        return LOCAL.try_with(f).ok();
    }
    PRIVATE
        .try_with(|local| {
            let mut labs = local.borrow_mut();
            let lab = labs
                .entry(gc.policy_identity)
                .or_insert_with(|| Arc::new(OldLab::new()));
            f(lab)
        })
        .ok()
}

pub(super) fn span_for(payload_size: usize) -> Option<(usize, usize)> {
    let total = GC_HEADER_SIZE.checked_add(payload_size)?;
    Some((
        total,
        total.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?,
    ))
}

impl OldLab {
    fn bump(
        &self,
        inner: &mut Inner,
        phase: &std::sync::atomic::AtomicU8,
        metadata: raw_heap::TraceMetadata,
        span: usize,
    ) -> Option<HeapObject> {
        let raw_heap::TraceMetadata {
            type_id,
            layout_id,
            gc_ref_mask,
            payload_size,
        } = metadata;
        let total = GC_HEADER_SIZE + payload_size;
        if !inner.active || inner.limit - inner.cursor < span {
            return None;
        }
        let offset = inner.cursor;
        inner.cursor += span;
        let raw = (inner.base + offset) as *mut u8;
        // SAFETY: [offset, offset + span) lies in this buffer's exclusively
        // carved, unpublished part of a live region.
        unsafe { std::ptr::write_bytes(raw, 0, span) };
        let object = HeapObject::initialize_at(
            raw,
            total,
            type_id,
            layout_id,
            gc_ref_mask,
            GC_GENERATION_OLD,
        )
        .expect("carved region span is non-null");
        let granule = offset / GC_REGION_MARK_GRANULE;
        // Same publication order as `OldRegion::allocate_object`.
        if phase.load(Ordering::Acquire) != 0 {
            inner.marks.as_ref().expect("active buffer").set(granule);
        }
        inner.starts.as_ref().expect("active buffer").set(granule);
        if inner.sweep_pending {
            object.begin_trace();
        }
        inner.pending.push((offset, span));
        inner.pending_live += total;
        self.bytes.fetch_add(total as u64, Ordering::Release);
        self.objects.fetch_add(1, Ordering::Release);
        Some(object)
    }
}

/// Whether this thread can still use its buffer; false during TLS teardown,
/// where allocation falls back to the per-object locked path.
pub(super) fn available() -> bool {
    LOCAL.try_with(|_| ()).is_ok()
}

pub(super) fn available_in(owner: &crate::runtime::Runtime) -> bool {
    if std::ptr::eq(owner, crate::runtime::current()) {
        available()
    } else {
        PRIVATE.try_with(|_| ()).is_ok()
    }
}

/// Allocation from this thread's active buffer without the heap mutex.
pub(super) fn try_allocate(
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
    total: usize,
    span: usize,
) -> Option<HeapObject> {
    LOCAL
        .try_with(|lab| {
            let mut inner = lab.inner.lock().unwrap();
            lab.bump(
                &mut inner,
                crate::runtime::current().mark_phase(),
                raw_heap::TraceMetadata {
                    type_id,
                    layout_id,
                    gc_ref_mask,
                    payload_size: total - GC_HEADER_SIZE,
                },
                span,
            )
        })
        .ok()
        .flatten()
}

pub(super) fn try_allocate_in(
    runtime: &crate::runtime::Runtime,
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
    total: usize,
    span: usize,
) -> Option<HeapObject> {
    if std::ptr::eq(runtime, crate::runtime::current()) {
        return try_allocate(type_id, layout_id, gc_ref_mask, total, span);
    }
    let owner = &*runtime.gc;
    with_local_in(owner, |lab| {
        let mut inner = lab.inner.lock().unwrap();
        lab.bump(
            &mut inner,
            owner.mark_phase(),
            raw_heap::TraceMetadata {
                type_id,
                layout_id,
                gc_ref_mask,
                payload_size: total - GC_HEADER_SIZE,
            },
            span,
        )
    })
    .flatten()
}

/// Retire this thread's buffer, carve a new one, and allocate the object in
/// it. Returns `None` when no region can supply `span` within the budget.
/// Callers check [`available`] first.
pub(super) fn refill_and_allocate(
    state: &mut GcState,
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
    total: usize,
    span: usize,
) -> Option<HeapObject> {
    refill_and_allocate_in(
        crate::runtime::current(),
        state,
        type_id,
        layout_id,
        gc_ref_mask,
        total,
        span,
    )
}

pub(super) fn refill_and_allocate_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
    total: usize,
    span: usize,
) -> Option<HeapObject> {
    with_local_in(&owner.gc, |lab| {
        RuntimePathObservation::old_lab_lookup(|| &owner.gc.path_observation);
        let key = Arc::as_ptr(lab) as usize;
        let index = match state.old_lab_indices.get(&key).copied() {
            Some(index) => {
                flush_in(&owner.gc, state, index, true);
                index
            }
            None => {
                state.old_lab_indices.insert(key, state.old_labs.len());
                state.old_labs.push(OldLabRecord {
                    lab: lab.clone(),
                    observed_bytes: lab.bytes.load(Ordering::Acquire),
                    observed_objects: lab.objects.load(Ordering::Acquire),
                });
                state.old_labs.len() - 1
            }
        };
        let (region, offset, len, reused) = carve_in(owner, state, span)?;
        let region_ref = &mut state.old_regions[region];
        region_ref.active_labs += 1;
        let mut inner = lab.inner.lock().unwrap();
        *inner = Inner {
            active: true,
            region,
            base: region_ref.start(),
            cursor: offset,
            limit: offset + len,
            reused,
            sweep_pending: region_ref.sweep_pending,
            starts: Some(region_ref.mark_bitmap.bits.clone()),
            marks: Some(region_ref.concurrent_marks.clone()),
            pending: std::mem::take(&mut inner.pending),
            pending_index: 0,
            pending_live: 0,
        };
        let object = lab.bump(
            &mut inner,
            owner.gc.mark_phase(),
            raw_heap::TraceMetadata {
                type_id,
                layout_id,
                gc_ref_mask,
                payload_size: total - GC_HEADER_SIZE,
            },
            span,
        );
        drop(inner);
        state.old_lab_pending.insert(index);
        sync_record_in(&owner.gc, state, index);
        object
    })
    .flatten()
}

/// Reserve a carve of at least `span` bytes. Prefers a span of at least
/// `OLD_LAB_MIN_BYTES` from the largest-available region, takes what the
/// largest region still offers for `span`, and otherwise reserves a region.
fn carve_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    span: usize,
) -> Option<(usize, usize, usize, bool)> {
    while state
        .old_region_candidates
        .last()
        .is_some_and(|&(_, index)| state.old_regions[index].sweep_quarantined)
    {
        // Each stale candidate is discarded at most once during the sweep.
        state.old_region_candidates.pop_last();
    }
    if let Some(&(available, index)) = state.old_region_candidates.last()
        && available >= span
    {
        let minimum = OLD_LAB_MIN_BYTES.max(span).min(available);
        let (offset, len, reused) = state.old_regions[index]
            .carve(minimum, OLD_LAB_BYTES)
            .expect("candidate offers its indexed available span");
        let available = state.old_regions[index].available_span();
        state.old_region_candidates.pop_last();
        if available >= GC_HEADER_SIZE {
            state.old_region_candidates.insert((available, index));
        }
        return Some((index, offset, len, reused));
    }
    let mut region = reserve_old_region_in(&owner.gc, state, RegionKind::Old, GC_OLD_REGION_SIZE)?;
    let (offset, len, reused) = region.carve(span, OLD_LAB_BYTES)?;
    let index = state.old_regions.len();
    state.old_addresses.insert(region.base as usize, index);
    state.old_regions.push(region);
    state.old_reserved_bytes += GC_OLD_REGION_SIZE;
    note_reservation_growth_in(owner, state);
    index_old_region(state, index);
    Some((index, offset, len, reused))
}

/// Private registration retirement releases its TLS cache while the runtime
/// still owns the heap. Other runtimes' buffers on this thread stay untouched.
pub(super) fn retire_private_current_in(owner: &crate::runtime::Runtime) {
    if std::ptr::eq(owner, crate::runtime::current()) {
        return;
    }
    let local = PRIVATE
        .try_with(|labs| labs.borrow_mut().remove(&owner.gc.policy_identity))
        .ok()
        .flatten();
    let Some(lab) = local else {
        return;
    };
    let mut state = owner.gc.heap.lock().unwrap();
    if let Some(index) = state
        .old_lab_indices
        .get(&(Arc::as_ptr(&lab) as usize))
        .copied()
    {
        flush_in(&owner.gc, &mut state, index, true);
        remove_record(&mut state, index);
    }
}

/// `LOCAL` belongs to the runtime this thread serves. Before the thread
/// starts serving another runtime, retire the buffer from `previous` so it can
/// neither keep carving `previous`'s regions on behalf of the next runtime nor
/// be flushed by `previous`'s collector while the next runtime uses it.
pub(crate) fn retire_served_in(previous: &crate::runtime::Runtime) {
    let Ok(lab) = LOCAL.try_with(Arc::clone) else {
        return;
    };
    // Heap records retain their buffer; only `LOCAL` and this clone remain
    // for a buffer that was never registered, so skip `previous`'s heap.
    if Arc::strong_count(&lab) == 2 {
        return;
    }
    let mut state = previous.gc.heap.lock().unwrap();
    if let Some(index) = state
        .old_lab_indices
        .get(&(Arc::as_ptr(&lab) as usize))
        .copied()
    {
        flush_in(&previous.gc, &mut state, index, true);
        remove_record(&mut state, index);
    }
}

/// Runtime destruction needs payload finalization, not a new region index.
/// Visit each still-unindexed object once and invalidate every cached buffer
/// before the runtime releases its regions.
pub(super) fn finalize_unindexed(state: &GcState, mut finalize: impl FnMut(HeapObject)) {
    for record in &state.old_labs {
        let mut inner = record.lab.inner.lock().unwrap_or_else(|p| p.into_inner());
        for &(offset, _) in &inner.pending[inner.pending_index..] {
            let object = HeapObject::from_raw((inner.base + offset) as *mut GcHeader)
                .expect("pending buffer object has an initialized header");
            finalize(object);
        }
        inner.active = false;
        inner.pending.clear();
        inner.pending_index = 0;
        inner.pending_live = 0;
    }
}

pub(super) fn forget_private_owner(identity: u64) {
    let _ = PRIVATE.try_with(|labs| labs.borrow_mut().remove(&identity));
}

/// Merge one buffer's allocation counters into the heap totals. Its objects
/// count as runtime (slow-path) old-region allocations, as before buffering.
fn sync_record(state: &mut GcState, index: usize) {
    sync_record_in(runtime(), state, index);
}

fn sync_record_in(owner: &GcRuntime, state: &mut GcState, index: usize) {
    let record = &mut state.old_labs[index];
    let bytes = record.lab.bytes.load(Ordering::Acquire);
    let objects = record.lab.objects.load(Ordering::Acquire);
    let byte_delta = bytes.saturating_sub(record.observed_bytes);
    let object_delta = objects.saturating_sub(record.observed_objects);
    record.observed_bytes = bytes;
    record.observed_objects = objects;
    add_counts(owner, state, byte_delta, object_delta);
}

fn add_counts(owner: &GcRuntime, state: &mut GcState, bytes: u64, objects: u64) {
    owner.headroom.consume(bytes as usize);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
    state.total_allocs = state.total_allocs.saturating_add(objects);
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(objects);
    state.old_region_allocations = state.old_region_allocations.saturating_add(objects);
}

/// Bytes-only merge for allocation-policy checks: one atomic load per buffer,
/// never a buffer lock, so policy sampling cannot stall an allocating owner.
pub(super) fn sync_bytes_in(owner: &GcRuntime, state: &mut GcState) {
    let mut bytes = 0u64;
    for record in &mut state.old_labs {
        let total = record.lab.bytes.load(Ordering::Acquire);
        bytes = bytes.saturating_add(total.saturating_sub(record.observed_bytes));
        record.observed_bytes = total;
    }
    owner.headroom.consume(bytes as usize);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
}

/// Index one buffer's pending objects into its region, keeping it active.
/// O(k log A) for k pending objects in a region of A indexed objects.
fn flush_in(owner: &GcRuntime, state: &mut GcState, index: usize, deactivate: bool) {
    {
        let GcState {
            old_labs,
            old_regions,
            old_addresses,
            old_region_reuses,
            ..
        } = &mut *state;
        let mut inner = old_labs[index].lab.inner.lock().unwrap();
        if inner.active || inner.pending_index < inner.pending.len() {
            inner.region = old_addresses.exact(inner.base).expect("live buffer region");
        }
        if inner.pending_index < inner.pending.len() {
            let region = &mut old_regions[inner.region];
            if inner.reused {
                *old_region_reuses = old_region_reuses
                    .saturating_add((inner.pending.len() - inner.pending_index) as u64);
            }
            for &(offset, span) in &inner.pending[inner.pending_index..] {
                region.allocations.insert(offset, span);
            }
            region.live_bytes = region.live_bytes.saturating_add(inner.pending_live);
            inner.pending.clear();
            inner.pending_index = 0;
            inner.pending_live = 0;
        }
        if deactivate && inner.active {
            // The unused tail [cursor, limit) stays a gap below `used` until
            // the region's next sweep rebuilds its free spans.
            inner.active = false;
            inner.starts = None;
            inner.marks = None;
            old_regions[inner.region].active_labs -= 1;
        }
    }
    // Objects counted after sweep could otherwise be freed before their bytes
    // were added, underflowing `allocated_bytes`.
    sync_record_in(owner, state, index);
    if deactivate {
        state.old_lab_pending.remove(index);
    }
}

fn retire(state: &mut GcState, index: usize) {
    flush_in(runtime(), state, index, true);
}

/// Index every pending object, keeping buffers active. Records of exited
/// threads are retired and dropped. O(T + pending objects).
pub(super) fn flush_all(state: &mut GcState) {
    flush_all_in(runtime(), state);
}

pub(super) fn flush_all_in(owner: &GcRuntime, state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        flush_in(owner, state, index, false);
    }
    prune_in(owner, state);
}

/// Retire every buffer; the next allocation of each owner refills under the
/// heap mutex. O(T + pending objects).
pub(super) fn retire_all(state: &mut GcState) {
    retire_all_in(runtime(), state);
}

pub(super) fn retire_all_in(owner: &GcRuntime, state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        flush_in(owner, state, index, true);
    }
    prune_in(owner, state);
}

/// Freeze each active buffer and index one pending entry per admitted chunk.
/// A rejected entry stays in its original Vec, with a durable indexed-prefix
/// cursor. Mutator refill/full flush consumes only the remaining suffix.
pub(super) fn retire_bounded(
    state: &mut GcState,
    budget: &mut old_cycle::RemarkBudget,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> bool {
    retire_bounded_in(runtime(), state, budget, work)
}

pub(super) fn retire_bounded_in(
    owner: &GcRuntime,
    state: &mut GcState,
    budget: &mut old_cycle::RemarkBudget,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> bool {
    retire_bounded_with_clock(
        owner,
        state,
        budget,
        work,
        crate::gc_telemetry::timestamp_ns,
    )
}

fn retire_bounded_with_clock(
    owner: &GcRuntime,
    state: &mut GcState,
    budget: &mut old_cycle::RemarkBudget,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
    mut now: impl FnMut() -> u64,
) -> bool {
    while let Some(index) = state.old_lab_pending.front() {
        if !budget.reserve(1, 0, now()) {
            return false;
        }
        let complete = {
            let GcState {
                old_labs,
                old_regions,
                old_addresses,
                old_region_reuses,
                ..
            } = &mut *state;
            let mut inner = old_labs[index].lab.inner.lock().unwrap();
            if !inner.active && inner.pending_index == inner.pending.len() {
                drop(inner);
                state.old_lab_pending.remove(index);
                continue;
            }
            if !budget.reserve(0, std::mem::size_of::<(usize, usize)>() as u64, now()) {
                return false;
            }
            inner.region = old_addresses.exact(inner.base).expect("live buffer region");
            let region = &mut old_regions[inner.region];
            if inner.active {
                // Revocation precedes indexing: a resumed owner must refill,
                // which also flushes this suffix before replacing the carve.
                inner.active = false;
                inner.starts = None;
                inner.marks = None;
                region.active_labs -= 1;
            }
            while inner.pending_index < inner.pending.len() {
                // One tuple and immutable header, never the carved payload.
                if !budget.reserve(
                    2,
                    (std::mem::size_of::<(usize, usize)>() + GC_HEADER_SIZE) as u64,
                    now(),
                ) {
                    break;
                }
                let (offset, span) = inner.pending[inner.pending_index];
                let object = HeapObject::from_raw((inner.base + offset) as *mut GcHeader)
                    .expect("pending allocation has a valid header");
                let logical_bytes = object.size();
                assert!(
                    region.allocations.insert(offset, span).is_none(),
                    "buffer indexed twice"
                );
                region.live_bytes = region.live_bytes.saturating_add(logical_bytes);
                inner.pending_live -= logical_bytes;
                if inner.reused {
                    *old_region_reuses = old_region_reuses.saturating_add(1);
                }
                inner.pending_index += 1;
                work.metadata_objects += 1;
                work.metadata_bytes +=
                    (std::mem::size_of::<(usize, usize)>() + GC_HEADER_SIZE) as u64;
            }
            let complete = inner.pending_index == inner.pending.len();
            if complete {
                assert_eq!(inner.pending_live, 0);
                inner.pending.clear();
                inner.pending_index = 0;
            }
            complete
        };
        sync_record_in(owner, state, index);
        if !complete {
            return false;
        }
        state.old_lab_pending.remove(index);
    }
    // Exited empty records are pruned by the existing post-resume sweep owner,
    // not an extra unchunked registry walk inside bounded normal remark.
    budget.reserve(0, 0, now())
}

/// Merge logical counters without enumerating pending objects. Initial mark
/// uses start-bit views and needs accounting, not per-object region indexes.
pub(super) fn sync_records_in(owner: &GcRuntime, state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        sync_record_in(owner, state, index);
    }
}

/// Retire buffers carved from one region before its sweep visit. The region's
/// count skips the registry scan for the usual region with no buffer.
pub(super) fn retire_region(owner: &GcRuntime, state: &mut GcState, region: usize) {
    if state.old_regions[region].active_labs == 0 {
        return;
    }
    for index in 0..state.old_labs.len() {
        let in_region = {
            let inner = state.old_labs[index].lab.inner.lock().unwrap();
            inner.active && inner.base == state.old_regions[region].start()
        };
        if in_region {
            flush_in(owner, state, index, true);
        }
    }
}

/// Drop records of exited threads: only the registry still owns their
/// buffer. Retiring first indexes their objects and releases the region count.
fn prune(state: &mut GcState) {
    prune_in(runtime(), state);
}

fn prune_in(owner: &GcRuntime, state: &mut GcState) {
    let mut index = 0;
    while index < state.old_labs.len() {
        if Arc::strong_count(&state.old_labs[index].lab) == 1 {
            flush_in(owner, state, index, true);
            remove_record(state, index);
        } else {
            index += 1;
        }
    }
}

// The caller has indexed and deactivated this record under the heap lock.
// Repair the moved record's index and pending bit in constant expected time.
fn remove_record(state: &mut GcState, index: usize) {
    let last = state.old_labs.len() - 1;
    let moved_pending = state.old_lab_pending.remove(last);
    let removed = state.old_labs.swap_remove(index);
    state
        .old_lab_indices
        .remove(&(Arc::as_ptr(&removed.lab) as usize));
    if index != last {
        state
            .old_lab_indices
            .insert(Arc::as_ptr(&state.old_labs[index].lab) as usize, index);
    }
    if index != last && moved_pending {
        state.old_lab_pending.insert(index);
    }
}

/// Reset owns all region storage: retire buffers before region objects are
/// finalized, then forget every registration.
pub(super) fn reset(state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        retire(state, index);
    }
    state.old_labs.clear();
    state.old_lab_indices.clear();
    state.old_lab_pending = Default::default();
}

#[cfg(test)]
pub(super) fn local_pending_for_test() -> usize {
    LOCAL.with(|lab| {
        let inner = lab.inner.lock().unwrap();
        inner.pending.len() - inner.pending_index
    })
}

#[cfg(test)]
#[path = "old_lab_tests.rs"]
mod tests;
