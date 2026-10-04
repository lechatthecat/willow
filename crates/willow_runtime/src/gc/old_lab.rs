//! Per-thread old-generation allocation buffers for runtime-side objects
//! (willow-jz15.53). A thread carves a span of up to `OLD_LAB_BYTES` from an
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
//! - An active buffer's region index stays valid: regions are reindexed only
//!   by `sweep::finish` and reset, which retire every buffer first.
//! - An active buffer's `sweep_pending` copy equals its region's flag:
//!   `sweep::prepare` retires every buffer before setting the flags, and
//!   `sweep_region` retires the region's buffers before visiting it.
//! - Mark closure retires every buffer before clearing `GC_MARK_PHASE`, so no
//!   buffer allocation observes phase 0 while its region still awaits a
//!   pending sweep without `sweep_pending`.
//! - Every pending object is flushed into its region before any walk of
//!   `allocations` (sweep, epoch capture, stopped marking, validation, reset).
use super::*;

/// Carve size: four generated TLAB chunks (half an old region). Runtime-heavy
/// pipelines refill while the sweeper holds the heap mutex region by region,
/// so a quarter of the refill rate measurably cuts blocking (willow-jz15.53
/// audit); an idle buffer pins at most this much per thread until retirement.
pub(super) const OLD_LAB_BYTES: usize = 4 * GC_TLAB_CHUNK_SIZE;
/// Smallest carve preferred from an existing region. Smaller holes stay for
/// exact-fit promotion and large runtime allocations.
const OLD_LAB_MIN_BYTES: usize = 1024;

#[cfg(test)]
pub(super) static OLD_LABS_FOR_TEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Unit tests that inspect per-object region metadata opt in explicitly.
pub(super) fn enabled() -> bool {
    #[cfg(test)]
    {
        OLD_LABS_FOR_TEST.load(Ordering::Acquire)
    }
    #[cfg(not(test))]
    {
        true
    }
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
    /// Unindexed `(offset, span)` objects of the current carve, ascending.
    pending: Vec<(usize, usize)>,
    pending_live: usize,
}

/// Heap-mutex-owned bookkeeping for one registered buffer.
pub(super) struct OldLabRecord {
    lab: Arc<OldLab>,
    observed_bytes: u64,
    observed_objects: u64,
}

thread_local! {
    static LOCAL: Arc<OldLab> = Arc::new(OldLab {
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
            pending_live: 0,
        }),
        bytes: AtomicU64::new(0),
        objects: AtomicU64::new(0),
    });
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
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        total: usize,
        span: usize,
    ) -> Option<HeapObject> {
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
        if GC_MARK_PHASE.load(Ordering::Acquire) != 0 {
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
            lab.bump(&mut inner, type_id, layout_id, gc_ref_mask, total, span)
        })
        .ok()
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
    LOCAL.with(|lab| {
        let index = match state.old_labs.iter().position(|r| Arc::ptr_eq(&r.lab, lab)) {
            Some(index) => {
                retire(state, index);
                index
            }
            None => {
                state.old_labs.push(OldLabRecord {
                    lab: lab.clone(),
                    observed_bytes: lab.bytes.load(Ordering::Acquire),
                    observed_objects: lab.objects.load(Ordering::Acquire),
                });
                state.old_labs.len() - 1
            }
        };
        let (region, offset, len, reused) = carve(state, span)?;
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
            pending_live: 0,
        };
        let object = lab.bump(&mut inner, type_id, layout_id, gc_ref_mask, total, span);
        drop(inner);
        sync_record(state, index);
        object
    })
}

/// Reserve a carve of at least `span` bytes. Prefers a span of at least
/// `OLD_LAB_MIN_BYTES` from the largest-available region, takes what the
/// largest region still offers for `span`, and otherwise reserves a region.
fn carve(state: &mut GcState, span: usize) -> Option<(usize, usize, usize, bool)> {
    while state
        .old_region_candidates
        .peek()
        .is_some_and(|&(_, index)| state.old_regions[index].sweep_quarantined)
    {
        // Each stale candidate is discarded at most once during the sweep.
        state.old_region_candidates.pop();
    }
    if let Some(&(available, index)) = state.old_region_candidates.peek()
        && available >= span
    {
        let minimum = OLD_LAB_MIN_BYTES.max(span).min(available);
        let (offset, len, reused) = state.old_regions[index]
            .carve(minimum, OLD_LAB_BYTES)
            .expect("candidate offers its indexed available span");
        let available = state.old_regions[index].available_span();
        let mut entry = state
            .old_region_candidates
            .peek_mut()
            .expect("candidate exists");
        if available >= GC_HEADER_SIZE {
            *entry = (available, index);
        } else {
            std::collections::binary_heap::PeekMut::pop(entry);
        }
        return Some((index, offset, len, reused));
    }
    if !can_reserve(state, GC_OLD_REGION_SIZE) {
        return None;
    }
    let mut region = OldRegion::new(RegionKind::Old, GC_OLD_REGION_SIZE)?;
    let (offset, len, reused) = region.carve(span, OLD_LAB_BYTES)?;
    let index = state.old_regions.len();
    state.old_addresses.insert(region.base as usize, index);
    state.old_regions.push(region);
    state.old_reserved_bytes += GC_OLD_REGION_SIZE;
    note_reservation_growth(state);
    index_old_region(state, index);
    Some((index, offset, len, reused))
}

/// Merge one buffer's allocation counters into the heap totals. Its objects
/// count as runtime (slow-path) old-region allocations, as before buffering.
fn sync_record(state: &mut GcState, index: usize) {
    let record = &mut state.old_labs[index];
    let bytes = record.lab.bytes.load(Ordering::Acquire);
    let objects = record.lab.objects.load(Ordering::Acquire);
    let byte_delta = bytes.saturating_sub(record.observed_bytes);
    let object_delta = objects.saturating_sub(record.observed_objects);
    record.observed_bytes = bytes;
    record.observed_objects = objects;
    add_counts(state, byte_delta, object_delta);
}

fn add_counts(state: &mut GcState, bytes: u64, objects: u64) {
    consume_headrooms(bytes as usize);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
    state.total_allocs = state.total_allocs.saturating_add(objects);
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(objects);
    state.old_region_allocations = state.old_region_allocations.saturating_add(objects);
    if state.allocated_bytes >= state.threshold_bytes {
        state.threshold_bytes = state.threshold_bytes.saturating_mul(2);
    }
}

/// Bytes-only merge for allocation-policy checks: one atomic load per buffer,
/// never a buffer lock, so policy sampling cannot stall an allocating owner.
pub(super) fn sync_bytes(state: &mut GcState) {
    let mut bytes = 0u64;
    for record in &mut state.old_labs {
        let total = record.lab.bytes.load(Ordering::Acquire);
        bytes = bytes.saturating_add(total.saturating_sub(record.observed_bytes));
        record.observed_bytes = total;
    }
    consume_headrooms(bytes as usize);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
}

/// Index one buffer's pending objects into its region, keeping it active.
/// O(k log A) for k pending objects in a region of A indexed objects.
fn flush(state: &mut GcState, index: usize, deactivate: bool) {
    {
        let GcState {
            old_labs,
            old_regions,
            old_region_reuses,
            ..
        } = &mut *state;
        let mut inner = old_labs[index].lab.inner.lock().unwrap();
        if !inner.pending.is_empty() {
            let region = &mut old_regions[inner.region];
            if inner.reused {
                *old_region_reuses = old_region_reuses.saturating_add(inner.pending.len() as u64);
            }
            for &(offset, span) in &inner.pending {
                region.allocations.insert(offset, span);
            }
            region.live_bytes = region.live_bytes.saturating_add(inner.pending_live);
            inner.pending.clear();
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
    sync_record(state, index);
}

fn retire(state: &mut GcState, index: usize) {
    flush(state, index, true);
}

/// Index every pending object, keeping buffers active. Records of exited
/// threads are retired and dropped. O(T + pending objects).
pub(super) fn flush_all(state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        flush(state, index, false);
    }
    prune(state);
}

/// Retire every buffer; the next allocation of each owner refills under the
/// heap mutex. O(T + pending objects).
pub(super) fn retire_all(state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        retire(state, index);
    }
    prune(state);
}

/// Retire buffers carved from one region before its sweep visit. The region's
/// count skips the registry scan for the usual region with no buffer.
pub(super) fn retire_region(state: &mut GcState, region: usize) {
    if state.old_regions[region].active_labs == 0 {
        return;
    }
    for index in 0..state.old_labs.len() {
        let in_region = {
            let inner = state.old_labs[index].lab.inner.lock().unwrap();
            inner.active && inner.region == region
        };
        if in_region {
            retire(state, index);
        }
    }
}

/// Drop records of exited threads: only the registry still owns their
/// buffer. Retiring first indexes their objects and releases the region count.
fn prune(state: &mut GcState) {
    let mut index = 0;
    while index < state.old_labs.len() {
        if Arc::strong_count(&state.old_labs[index].lab) == 1 {
            retire(state, index);
            state.old_labs.swap_remove(index);
        } else {
            index += 1;
        }
    }
}

/// Reset owns all region storage: retire buffers before region objects are
/// finalized, then forget every registration.
pub(super) fn reset(state: &mut GcState) {
    for index in 0..state.old_labs.len() {
        retire(state, index);
    }
    state.old_labs.clear();
}

#[cfg(test)]
pub(super) fn local_pending_for_test() -> usize {
    LOCAL.with(|lab| lab.inner.lock().unwrap().pending.len())
}

#[cfg(test)]
#[path = "old_lab_tests.rs"]
mod tests;
