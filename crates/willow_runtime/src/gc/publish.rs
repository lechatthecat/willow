//! Task-owned young regions and publication.
//!
//! Every young object (TLAB and survivor chunks) belongs to the task that
//! allocated it. Chunks record ownership as `(offset, owner)` segments
//! (`GcState::chunk_owners`): a worker thread's TLAB is shared by the
//! tasks it runs, so each task switch that moved the allocation cursor starts
//! a new segment, and survivor copies keep their source's owner. A migrating
//! task's earlier segments stay its own (handed over); a task that ends,
//! is cancelled or panics hands the cursor to owner zero (returned).
//!
//! Invariant: a task's young objects are referenced only from its own stack,
//! frames and young objects, or from its private old objects. A value that
//! becomes reachable by another task is *published* first: the whole graph
//! reachable from it that is not yet shared gets the never-cleared SHARED
//! header mark, which takes its young objects out of their task's private
//! young set where they lie (no copy, no generation change). Minor
//! collections stop the world and see every holder, so a published young
//! object then ages, moves and tenures like any other young object, keeping
//! its mark on every copy; short-lived messages die young without pinning
//! or fragmenting chunks. Only the objects a parked publication still holds
//! by address stay in place (`Parked`). Publication points:
//! channel sends, spawn captures and arguments, task results, lock and
//! atomic protected state, globals, runtime root slots, native boundaries and
//! every store into a shared object (`barrier::write_barrier_in`).
//!
//! Costs: a shared value is recognised from its own header, so republishing
//! is O(1) and a publication is linear in the objects it newly marks. The
//! traversal is iterative (no recursion depth), parks at a requested stop
//! when its caller allows it, and changes no reference or generation, so
//! concurrent SATB marking and sweeping observe the same graph. Counters
//! accumulate in the mutator's private log (`mutator_log::PublicationLog`),
//! so a publication or republication touches no shared cache line.
//!
//! Young values stored into a private old object are not published; the
//! generational barrier logs them in the storing mutator's lock-free log as
//! before. Publishing that old object later publishes those children; as
//! for any old-to-young edge, the next minor drops the remembered entry once
//! the owner has no young child. A published young value stored into an old
//! object takes the same generational path.
use super::*;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64};

/// Where a value became reachable by other tasks. Counted per point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum PublicationPoint {
    ChannelSend,
    Spawn,
    TaskReturn,
    LockedState,
    Global,
    RuntimeRoot,
    NativeBoundary,
    SharedStore,
}

impl PublicationPoint {
    pub(crate) const ALL: [Self; 8] = [
        Self::ChannelSend,
        Self::Spawn,
        Self::TaskReturn,
        Self::LockedState,
        Self::Global,
        Self::RuntimeRoot,
        Self::NativeBoundary,
        Self::SharedStore,
    ];
}

/// Objects marked between checks for a requested stop.
const PARK_POLL_OBJECTS: u64 = 256;

/// Per-runtime publication state and counters.
#[derive(Default)]
pub(crate) struct Publication {
    calls: AtomicU64,
    republished: AtomicU64,
    handed_over: AtomicU64,
    marked_objects: AtomicU64,
    scanned_slots: AtomicU64,
    promoted_objects: AtomicU64,
    promoted_bytes: AtomicU64,
    promoted_died_bytes: AtomicU64,
    parks: AtomicU64,
    per_point: [AtomicU64; 8],
    /// Per parked thread: the published root and the shared objects whose
    /// children the parked publication has not scanned yet. The minor treats
    /// all of them as in-place roots (their unshared children move with
    /// updated slots); the verifier exempts the unscanned ones.
    parked: Mutex<HashMap<std::thread::ThreadId, Parked>>,
    /// Verifier only: owners of finished tasks whose young objects were
    /// handed over with their result (`finish_task_result_in`); the verifier
    /// treats them as owned by no task. Cleared by every minor, which
    /// re-owns all survivors.
    pub(super) handed_over_owners: Mutex<crate::id_hash::IdSet<u64>>,
    /// Test-only: publication points to skip (bit per point), so negative
    /// tests can show the verifier catches each missing publication.
    #[cfg(test)]
    skipped_points: std::sync::atomic::AtomicU8,
}

/// Snapshot of [`Publication`] counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicationStats {
    /// Publication calls that reached an unshared value.
    pub calls: u64,
    /// Calls whose value was already shared: O(1), nothing scanned.
    pub republished: u64,
    /// Finished tasks whose handle had not escaped: their result was handed
    /// over to the handle's holder unpublished, O(1).
    pub handed_over: u64,
    /// Objects newly marked shared.
    pub marked_objects: u64,
    /// Reference slots read while marking.
    pub scanned_slots: u64,
    /// Young objects published (taken out of their task's private young set
    /// in place), and their bytes.
    pub promoted_objects: u64,
    pub promoted_bytes: u64,
    /// Bytes of published young objects a later collection freed: dead at a
    /// minor while still young, or tenured and freed by a major.
    pub promoted_died_bytes: u64,
    /// Times a publication parked mid-way for a requested stop.
    pub parks: u64,
    /// Calls per [`PublicationPoint`], in `PublicationPoint::ALL` order.
    pub per_point: [u64; 8],
}

impl Publication {
    pub(crate) fn stats(&self) -> PublicationStats {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        PublicationStats {
            calls: load(&self.calls),
            republished: load(&self.republished),
            handed_over: load(&self.handed_over),
            marked_objects: load(&self.marked_objects),
            scanned_slots: load(&self.scanned_slots),
            promoted_objects: load(&self.promoted_objects),
            promoted_bytes: load(&self.promoted_bytes),
            promoted_died_bytes: load(&self.promoted_died_bytes),
            parks: load(&self.parks),
            per_point: std::array::from_fn(|index| load(&self.per_point[index])),
        }
    }

    pub(super) fn note_promoted_died(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.promoted_died_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Add one mutator's private counters. Called when its log is applied.
    pub(super) fn merge(&self, counts: &mutator_log::PublicationCounts) {
        let add = |counter: &AtomicU64, value: u64| {
            if value != 0 {
                counter.fetch_add(value, Ordering::Relaxed);
            }
        };
        add(&self.calls, counts.calls);
        add(&self.republished, counts.republished);
        add(&self.handed_over, counts.handed_over);
        add(&self.marked_objects, counts.marked);
        add(&self.scanned_slots, counts.slots);
        add(&self.promoted_objects, counts.promoted_objects);
        add(&self.promoted_bytes, counts.promoted_bytes);
        for (counter, &value) in self.per_point.iter().zip(&counts.per_point) {
            add(counter, value);
        }
    }

    /// Whether `address` is a shared object a parked publication has not
    /// finished scanning. Read only in a stop.
    pub(super) fn parked_contains(&self, address: usize) -> bool {
        self.parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|parked| parked.pending.contains(&address))
    }

    /// Every object of a parked publication (roots and unscanned objects),
    /// for the minor collection of the stop they parked at.
    pub(super) fn parked_objects(&self) -> Vec<usize> {
        self.parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .flat_map(|parked| std::iter::once(parked.root).chain(parked.pending.iter().copied()))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn set_point_skipped_for_test(&self, point: PublicationPoint, skipped: bool) {
        let bit = 1u8 << point as u8;
        if skipped {
            self.skipped_points.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.skipped_points.fetch_and(!bit, Ordering::Relaxed);
        }
    }
}

/// Counters of `owner`. The calling thread's private counters are merged
/// first; other mutators' merge at every stop and when they unregister.
/// Must not be called with the heap lock held.
pub(crate) fn stats_in(owner: &crate::runtime::Runtime) -> PublicationStats {
    mutator_log::flush_current_in(owner);
    owner.gc.publication.stats()
}

/// `WILLOW_GC_PUBLICATION_STATS=1`: one summary line on stderr with the
/// publication counters and the collector's cycle and pause totals.
pub(crate) fn report_publication_stats_if_requested() {
    let owner = crate::runtime::current();
    if !*owner.diagnostics.publication_stats {
        return;
    }
    let p = stats_in(owner);
    let mut gc = crate::gc_telemetry::WillowGcStatsV1::default();
    // SAFETY: The typed output owns one aligned, writable record for the call.
    unsafe { crate::gc_telemetry::willow_gc_stats_snapshot_v1(&mut gc) };
    let per_point = PublicationPoint::ALL
        .iter()
        .zip(p.per_point)
        .map(|(point, calls)| format!("{point:?}={calls}"))
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "[gc-publication] calls={} republished={} handed_over={} marked_objects={} scanned_slots={} promoted_objects={} promoted_bytes={} promoted_died_bytes={} parks={} per_point={} minor_cycles={} major_cycles={} pause_total_ns={} pause_max_ns={} minor_pause_ns={} major_pause_ns={} rss={}",
        p.calls,
        p.republished,
        p.handed_over,
        p.marked_objects,
        p.scanned_slots,
        p.promoted_objects,
        p.promoted_bytes,
        p.promoted_died_bytes,
        p.parks,
        per_point,
        gc.minor_cycles,
        gc.major_cycles,
        gc.pauses.total_ns,
        gc.pauses.max_ns,
        gc.minor_pauses.total_ns,
        gc.major_pauses.total_ns,
        gc.process_resident_bytes,
    );
}

thread_local! {
    /// Task running on this thread: task id plus one, zero for none.
    static TASK_OWNER: Cell<u64> = const { Cell::new(0) };
    /// Reused traversal buffers; taken (not borrowed) so a nested
    /// publication on this thread simply starts with fresh ones.
    static WORK: Cell<Vec<usize>> = const { Cell::new(Vec::new()) };
    static SLOTS: Cell<Vec<*mut *mut u8>> = const { Cell::new(Vec::new()) };
    /// Trace hooks of the runtime this thread last published in, keyed by
    /// runtime identity and registry generation (`REGISTRY_KEY`).
    static REGISTRY: Cell<Option<Arc<registry::MinorRegistrySnapshot>>> =
        const { Cell::new(None) };
    static REGISTRY_KEY: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    /// `FrameTest` memo carried between this thread's publications.
    static PLAIN_TYPE_ZERO: Cell<[usize; 2]> = const { Cell::new([0; 2]) };
}

/// The frame test of a traversal. Inline-traced generated objects share type
/// id zero with frames, so for them the layout fingerprint decides (an FNV
/// hash); the last two static type-zero descriptors proven not frames are
/// remembered, so a graph of one or two generated classes (a value and its
/// `Option` box, say) hashes once per thread. Only a static descriptor is
/// remembered: its address never names another layout.
struct FrameTest {
    plain: [usize; 2],
    /// Whether `plain` changed and must be stored back on drop.
    changed: bool,
}

impl FrameTest {
    #[inline(always)]
    fn load() -> Self {
        Self {
            plain: PLAIN_TYPE_ZERO.try_with(Cell::get).unwrap_or([0; 2]),
            changed: false,
        }
    }

    #[inline(always)]
    fn is_frame(&mut self, layout: &willow_abi::GcLayoutDescriptor, static_layout: bool) -> bool {
        let address = std::ptr::from_ref(layout) as usize;
        layout.type_id as u32 == root::ASYNC_FRAME_TYPE_ID
            && address != self.plain[0]
            && address != self.plain[1]
            && self.fingerprint(layout, static_layout)
    }

    #[inline(never)]
    fn fingerprint(
        &mut self,
        layout: &willow_abi::GcLayoutDescriptor,
        static_layout: bool,
    ) -> bool {
        if raw_heap::is_async_frame_layout(layout) {
            return true;
        }
        if static_layout {
            self.plain = [std::ptr::from_ref(layout) as usize, self.plain[0]];
            self.changed = true;
        }
        false
    }

    #[inline(always)]
    fn store(&self) {
        if self.changed {
            let _ = PLAIN_TYPE_ZERO.try_with(|cell| cell.set(self.plain));
        }
    }
}

/// Owner of the task running on this thread (task id plus one), or zero.
pub(crate) fn current_task_owner() -> u64 {
    TASK_OWNER.try_with(Cell::get).unwrap_or(0)
}

/// Task owner for a scheduler task id.
pub(crate) fn task_owner(task_id: u64) -> u64 {
    task_id.wrapping_add(1)
}

/// This thread's allocation cursor, or zero without an installed chunk.
fn current_cursor() -> usize {
    let state = runtime_tlab::willow_gc_current_tlab();
    // SAFETY: this thread's TLS allocation block lives until thread exit.
    unsafe { (*state).cursor.load(Ordering::Relaxed) }
}

/// The scheduler switched this thread to `task` (task id plus one; zero when
/// it runs no task). Bytes allocated from the current cursor on belong to
/// `task`. O(1): a switch that allocated nothing since the previous one
/// overwrites that entry, so a task that never allocates adds nothing.
/// Inlined into every scheduler switch: one thread-local store and one
/// relaxed flag load. Only the verifier, the sole reader of chunk ownership,
/// pays for recording it (and for looking up the runtime).
#[inline]
pub(crate) fn switch_task_owner(task: u64) {
    let Ok(previous) = TASK_OWNER.try_with(|current| current.replace(task)) else {
        return;
    };
    if previous != task && OWNER_TRACKING.load(Ordering::Relaxed) {
        record_task_owner(crate::runtime::current(), task);
    }
}

/// [`switch_task_owner`] for an explicit runtime, deciding from its own
/// verifier setting.
#[cfg(test)]
pub(crate) fn switch_task_owner_in(owner: &crate::runtime::Runtime, task: u64) {
    let Ok(previous) = TASK_OWNER.try_with(|current| current.replace(task)) else {
        return;
    };
    if previous != task {
        record_task_owner(owner, task);
    }
}

/// Set once some runtime records chunk ownership (its publication verifier
/// is on); never cleared. A thread installs a chunk, and so evaluates this,
/// before it allocates, so no switch that owns allocations goes unrecorded.
static OWNER_TRACKING: AtomicBool = AtomicBool::new(false);

/// Whether `owner` records chunk ownership: its publication verifier is on.
pub(super) fn tracks_owners(owner: &crate::runtime::Runtime) -> bool {
    let on = owner.diagnostics.publication_verifier_enabled();
    if on && !OWNER_TRACKING.load(Ordering::Relaxed) {
        OWNER_TRACKING.store(true, Ordering::Relaxed);
    }
    on
}

#[cold]
#[inline(never)]
fn record_task_owner(owner: &crate::runtime::Runtime, task: u64) {
    if !owner.diagnostics.publication_verifier_enabled() {
        return;
    }
    if !owner.gc.tlab_ever_allocated.load(Ordering::Acquire) {
        return;
    }
    let cursor = current_cursor();
    if cursor == 0 {
        return;
    }
    match mutator_log::bound_in(&owner.gc) {
        Some(log) => {
            // SAFETY: the bound thread owns its log while running.
            let publication = unsafe { log.publication() };
            if publication.note_owner(cursor, task) {
                mutator_log::flush_publication(owner, publication);
            }
        }
        None => {
            let mut log = mutator_log::PublicationLog::default();
            let mut state = owner.gc.heap.lock().unwrap();
            log.start_chunk(current_chunk_base(&state, cursor));
            log.note_owner(cursor, task);
            log.apply(&mut state, &owner.gc.publication);
        }
    }
}

/// Base of the chunk this thread allocates from, for an unregistered thread
/// that applies owner entries directly.
fn current_chunk_base(state: &GcState, cursor: usize) -> usize {
    state
        .tlab_addresses
        .candidate(cursor.saturating_sub(1))
        .map_or(0, |index| state.tlab_chunks[index].base as usize)
}

/// Called with the heap lock held when this thread installs a new TLAB
/// chunk at `base`: apply owner entries of the previous chunk, then start
/// the new chunk owned by the running task.
pub(super) fn install_chunk_owner(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    index: usize,
) {
    let task = current_task_owner();
    let base = state.tlab_chunks[index].base as usize;
    if let Some(log) = mutator_log::bound_in(&owner.gc) {
        // SAFETY: the bound thread owns its log while running.
        let publication = unsafe { log.publication() };
        publication.apply(state, &owner.gc.publication);
        publication.start_chunk(base);
        // SAFETY: as above.
        unsafe { log.note_young_chunk(base) };
    }
    if tracks_owners(owner) {
        // A reused chunk base drops the owners of its previous life.
        state
            .chunk_owners
            .insert(base, super::region::OwnerSegments(vec![(0, task)]));
    }
}

fn registry_snapshot(owner: &crate::runtime::Runtime) -> Arc<registry::MinorRegistrySnapshot> {
    let (key, cached) = take_registry(owner);
    let snapshot = cached.unwrap_or_else(|| registry::minor_registry_snapshot_in(&owner.gc));
    put_registry(key, Arc::clone(&snapshot));
    snapshot
}

/// Trace hooks for one publication. Most published objects are generated
/// (their slots are in the layout descriptor); only a type id inside the
/// runtime's hooked range takes this thread's cached registry snapshot, moved
/// out of the cache and back on drop so the hot path makes no reference-count
/// update on the `Arc` every worker shares. A nested publication (a park in a
/// traversal) finds the cache empty and takes its own snapshot.
struct TraceHooks<'a> {
    owner: &'a crate::runtime::Runtime,
    min: u32,
    max: u32,
    /// The last hooked type looked up: a container graph repeats one runtime
    /// type (arrays, maps), so most lookups skip the registry map.
    last: Option<(u32, Option<registry::TraceFn>)>,
    frames: FrameTest,
    snapshot: Option<((u64, u64), Arc<registry::MinorRegistrySnapshot>)>,
}

impl<'a> TraceHooks<'a> {
    #[inline(always)]
    fn new(owner: &'a crate::runtime::Runtime) -> Self {
        let (min, max) = registry::trace_type_range_in(&owner.gc);
        Self {
            owner,
            min,
            max,
            last: None,
            frames: FrameTest::load(),
            snapshot: None,
        }
    }

    /// Whether `type_id` may have a hook. O(1), no memory access.
    #[inline(always)]
    fn may_have(&self, type_id: u32) -> bool {
        type_id >= self.min && type_id <= self.max
    }

    #[inline(always)]
    fn get(&mut self, type_id: u32) -> Option<registry::TraceFn> {
        if !self.may_have(type_id) {
            return None;
        }
        if let Some((last, trace)) = self.last
            && last == type_id
        {
            return trace;
        }
        let trace = self.snapshot().trace_hook(type_id);
        self.last = Some((type_id, trace));
        trace
    }

    #[cold]
    #[inline(never)]
    fn snapshot(&mut self) -> &registry::MinorRegistrySnapshot {
        let owner = self.owner;
        &self
            .snapshot
            .get_or_insert_with(|| {
                let (key, cached) = take_registry(owner);
                let snapshot =
                    cached.unwrap_or_else(|| registry::minor_registry_snapshot_in(&owner.gc));
                (key, snapshot)
            })
            .1
    }
}

impl Drop for TraceHooks<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.frames.store();
        if let Some((key, snapshot)) = self.snapshot.take() {
            put_registry(key, snapshot);
        }
    }
}

/// This runtime's registry key and the cached snapshot, if it is current.
/// One word moves out of the cache; the key stays.
#[inline]
fn take_registry(
    owner: &crate::runtime::Runtime,
) -> ((u64, u64), Option<Arc<registry::MinorRegistrySnapshot>>) {
    let gc = &*owner.gc;
    let key = (
        gc.policy_identity,
        gc.minor_registry_generation.load(Ordering::Acquire),
    );
    let cached = REGISTRY.try_with(Cell::take).ok().flatten().filter(|_| {
        REGISTRY_KEY
            .try_with(Cell::get)
            .is_ok_and(|cached| cached == key)
    });
    (key, cached)
}

#[inline]
fn put_registry(key: (u64, u64), snapshot: Arc<registry::MinorRegistrySnapshot>) {
    if let Ok(previous) = REGISTRY.try_with(|cached| cached.replace(Some(snapshot))) {
        // A nested publication may have cached one meanwhile.
        drop(previous);
        let _ = REGISTRY_KEY.try_with(|cached| cached.set(key));
    }
}

/// Async frames are a task's stack: never shared, never traversed. Their
/// reference slots are published individually where a frame hands values
/// over (spawn arguments, task results).
/// Inline-traced objects share type id zero with frames, so the layout kind
/// decides (one fingerprint hash, only for type id zero).
pub(super) fn is_frame(object: HeapObject) -> bool {
    object.type_id() == root::ASYNC_FRAME_TYPE_ID && object.is_async_frame()
}

/// Publish `value`: mark everything reachable from it that is not yet
/// shared, in place. `may_park` allows parking at a
/// requested stop mid-way; only callers whose own call site is a GC point
/// pass true (generated barrier call sites are not: they may hold unrooted
/// temporaries). O(1) for a null or already shared value.
/// A non-null value must start a live managed payload in `owner`. The mutator
/// publishing it owns the private portion of the reachable graph until it is shared.
#[inline]
pub(crate) fn publish_in(
    owner: &crate::runtime::Runtime,
    value: *mut u8,
    point: PublicationPoint,
    may_park: bool,
) {
    if let Some(object) = unshared_object(owner, value) {
        let park = if may_park {
            Park::Allowed
        } else {
            Park::Disabled
        };
        publish_unshared(owner, object, point, park);
    }
}

enum Park<'a> {
    Disabled,
    Allowed,
    WithSendOperands(&'a mut [*mut u8; 2]),
}

/// A blocking send still needs both ABI arguments after publication. Register
/// them only if traversal actually parks, then return their current addresses.
#[inline]
pub(crate) fn publish_channel_send(channel: *mut u8, value: *mut u8) -> [*mut u8; 2] {
    let owner = crate::runtime::current();
    let mut operands = [channel, value];
    if let Some(object) = unshared_object(owner, value) {
        publish_unshared(
            owner,
            object,
            PublicationPoint::ChannelSend,
            Park::WithSendOperands(&mut operands),
        );
    }
    operands
}

#[inline]
fn unshared_object(owner: &crate::runtime::Runtime, value: *mut u8) -> Option<HeapObject> {
    let object = HeapObject::from_payload(GcPayload::from_raw(value)?);
    if object.shared() {
        count_republished(owner);
        return None;
    }
    Some(object)
}

/// O(1) and uncontended: the bound mutator's private log, merged later.
#[inline]
fn count_republished(owner: &crate::runtime::Runtime) {
    match mutator_log::bound_in(&owner.gc) {
        // SAFETY: the bound thread owns its log while running.
        Some(log) => unsafe { log.publication() }.counts.republished += 1,
        None => {
            owner
                .gc
                .publication
                .republished
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Publish the values in the reference slots of the async frame `frame`
/// (`only`: just the slot at that payload byte offset). The frame itself is
/// the task's stack and stays private.
pub(crate) fn publish_frame_slots_in(
    owner: &crate::runtime::Runtime,
    frame: *mut u8,
    only: Option<usize>,
    point: PublicationPoint,
    may_park: bool,
) {
    let Some(payload) = GcPayload::from_raw(frame) else {
        return;
    };
    let object = HeapObject::from_payload(payload);
    let snapshot = registry_snapshot(owner);
    let mut slots = Vec::new();
    append_object_reference_slots(object, &snapshot.trace, &mut slots);
    let wanted = only.map(|offset| frame as usize + offset);
    for slot in slots {
        if slot.is_null() || wanted.is_some_and(|wanted| wanted != slot as usize) {
            continue;
        }
        // SAFETY: the frame is a live old object; trace metadata names its
        // reference slots. Reloaded per slot: a park may update it.
        let value = unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot) }.load(Ordering::Acquire);
        publish_in(owner, value, point, may_park);
    }
}

/// Payload word of frame data slot 0, the task result.
const FRAME_RESULT_WORD: usize =
    crate::async_frame::async_frame_slot_offset(0) / GC_STORAGE_WORD_BYTES;

/// The result slot of the async frame `frame` when it holds a reference.
fn frame_result_slot(frame: HeapObject) -> Option<*mut *mut u8> {
    frame
        .with_live_layout(|layout, _| layout.gc_ref_mask & (1 << FRAME_RESULT_WORD) != 0)
        .then(|| frame.payload_slot(FRAME_RESULT_WORD))
}

/// The task of `frame` finished (its result slot is written, its terminal
/// code not yet published). A handle that never escaped is held by one task
/// only, the sole reader of the result: the result is handed over to it
/// unpublished, O(1). Otherwise awaiters on any worker may read it, so it is
/// published first. A later escape of a handed-over handle publishes the
/// result then ([`escape_frame`]). A GC point: the frame stays rooted.
pub(crate) fn finish_task_result_in(owner: &crate::runtime::Runtime, task_id: u64, frame: *mut u8) {
    // SAFETY: The frame owner retains the initialized ABI status word through this atomic
    // access.
    if unsafe { crate::async_frame::frame_hand_over_result(frame.cast()) } {
        note_result_handed_over(owner, task_id);
    } else {
        publish_task_result_in(owner, frame);
    }
}

/// A finished task's result (frame data slot 0) becomes readable by the
/// awaiters its handle escaped to.
pub(super) fn publish_task_result_in(owner: &crate::runtime::Runtime, frame: *mut u8) {
    let Some(payload) = GcPayload::from_raw(frame) else {
        return;
    };
    let Some(slot) = frame_result_slot(HeapObject::from_payload(payload)) else {
        return;
    };
    // SAFETY: the rooted frame's result slot; its task has finished.
    let value = unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot) }.load(Ordering::Acquire);
    publish_in(owner, value, PublicationPoint::TaskReturn, true);
}

/// Count a handover; under the verifier, the finished task's young objects
/// now belong to the handle's holder, which no allocation segment names:
/// record the owner as handed over (verifier-only, cleared by the next minor,
/// which re-owns every survivor).
fn note_result_handed_over(owner: &crate::runtime::Runtime, task_id: u64) {
    match mutator_log::bound_in(&owner.gc) {
        // SAFETY: the bound thread owns its log while running.
        Some(log) => unsafe { log.publication() }.counts.handed_over += 1,
        None => {
            owner
                .gc
                .publication
                .handed_over
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if owner.diagnostics.publication_verifier_enabled() {
        owner
            .gc
            .publication
            .handed_over_owners
            .lock()
            .unwrap()
            .insert(task_owner(task_id));
    }
}

/// A publication reached the task handle `frame`. The frame stays private (it
/// is its task's stack) but other tasks may now read its result: returns that
/// result when the task already handed it over unpublished, so the caller
/// publishes it as part of the same graph. Cold: handles are rarely published;
/// one load once a handle has escaped.
#[cold]
#[inline(never)]
fn escape_frame(frame: HeapObject) -> Option<HeapObject> {
    // SAFETY: The descriptor identifies this still-live payload as an async frame during
    // publication.
    if !unsafe { crate::async_frame::frame_mark_result_escaped(frame.payload().as_ptr().cast()) } {
        return None;
    }
    let slot = frame_result_slot(frame)?;
    // SAFETY: the frame's result slot; its task has finished writing it.
    let value = unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot) }.load(Ordering::Acquire);
    GcPayload::from_raw(value).map(HeapObject::from_payload)
}

/// The first object of `object`'s handle chain that is not a frame and not
/// yet shared: a published handle publishes its handed-over result, which may
/// itself be a handle. Iterative; None when nothing is left to mark.
#[inline(always)]
fn past_frames(mut object: HeapObject, frames: &mut FrameTest) -> Option<HeapObject> {
    loop {
        if object.shared() {
            return None;
        }
        if !object.with_live_layout(|layout, is_static| frames.is_frame(layout, is_static)) {
            return Some(object);
        }
        object = escape_frame(object)?;
    }
}

#[inline(never)]
fn publish_unshared(
    owner: &crate::runtime::Runtime,
    object: HeapObject,
    point: PublicationPoint,
    park_mode: Park<'_>,
) {
    let gc = &*owner.gc;
    #[cfg(test)]
    if gc.publication.skipped_points.load(Ordering::Relaxed) & (1 << point as u8) != 0 {
        return;
    }
    let Some(log) = mutator_log::bound_in(gc) else {
        publish_unregistered(owner, object, point);
        return;
    };
    let mut hooks = TraceHooks::new(owner);
    let Some(object) = past_frames(object, &mut hooks.frames) else {
        return;
    };
    let mut counts = TraversalCounts::default();
    // A leaf (strings, scalar boxes: the common message) has no children:
    // mark it without a traversal.
    let card_owner = if has_no_slots(object, &hooks) {
        mark(object, &mut counts)
    } else {
        traverse(owner, object, &mut hooks, park_mode, &mut counts)
    };
    // SAFETY: the bound thread owns its log while running; the traversal
    // (and any park in it) holds no borrow of it.
    add_counts(&mut unsafe { log.publication() }.counts, &counts, point);
    if card_owner {
        invalidate_card_cache(owner);
    }
}

/// An unregistered thread is not stopped by a minor: hold the heap lock
/// across the whole publication so no collection observes a half-published
/// graph.
#[cold]
#[inline(never)]
fn publish_unregistered(
    owner: &crate::runtime::Runtime,
    object: HeapObject,
    point: PublicationPoint,
) {
    let gc = &*owner.gc;
    let mut hooks = TraceHooks::new(owner);
    let Some(object) = past_frames(object, &mut hooks.frames) else {
        return;
    };
    let mut counts = TraversalCounts::default();
    let mut state = gc.heap.lock().unwrap();
    let card_owner = traverse(owner, object, &mut hooks, Park::Disabled, &mut counts);
    drop(hooks);
    let mut log = mutator_log::PublicationLog::default();
    add_counts(&mut log.counts, &counts, point);
    log.apply(&mut state, &gc.publication);
    drop(state);
    if card_owner {
        invalidate_card_cache(owner);
    }
}

/// A card-cache entry for a then-private buffer must not let a later store
/// into the now shared buffer skip its barrier.
#[cold]
fn invalidate_card_cache(owner: &crate::runtime::Runtime) {
    owner.card_epoch().fetch_add(1, Ordering::AcqRel);
}

fn add_counts(
    into: &mut mutator_log::PublicationCounts,
    traversal: &TraversalCounts,
    point: PublicationPoint,
) {
    into.calls += 1;
    into.per_point[point as usize] += 1;
    into.marked += traversal.marked;
    into.slots += traversal.slots;
    into.promoted_objects += traversal.promoted_objects;
    into.promoted_bytes += traversal.promoted_bytes;
}

/// Whether `object` has no reference slots: no inline or bitmap slot and
/// no possible runtime trace hook. O(1), reads no registry.
#[inline(always)]
fn has_no_slots(object: HeapObject, hooks: &TraceHooks<'_>) -> bool {
    let metadata = object.trace_metadata();
    metadata.type_id != willow_abi::GC_BITMAP_TYPE_ID
        && metadata.inline_ref_mask() == 0
        && !hooks.may_have(metadata.type_id)
}

/// What one traversal marked and scanned; added to the mutator's
/// publication counters when it ends (or before it parks).
#[derive(Default)]
struct TraversalCounts {
    marked: u64,
    slots: u64,
    promoted_objects: u64,
    promoted_bytes: u64,
}

/// Mark `object` shared; the caller queues it for scanning. A young object
/// keeps its generation (`SHARED_PROMOTED` records that it was published
/// young); returns whether `object` is an old card-remembered buffer.
#[inline]
fn mark(object: HeapObject, counts: &mut TraversalCounts) -> bool {
    counts.marked += 1;
    if object.generation() == GC_GENERATION_OLD {
        object.set_shared(false);
        remembered::card_array_capacity(object).is_some()
    } else {
        object.set_shared(true);
        counts.promoted_objects += 1;
        counts.promoted_bytes += object.size() as u64;
        false
    }
}

/// Mark the graph of `root`; returns whether an old card-remembered buffer
/// became shared. Each object's layout descriptor is read once: it gives the
/// reference slots, the frame test, the size and whether to queue it.
/// Inlined into its two callers so the counts stay in registers.
#[inline(always)]
fn traverse(
    owner: &crate::runtime::Runtime,
    root: HeapObject,
    hooks: &mut TraceHooks<'_>,
    mut park_mode: Park<'_>,
    total: &mut TraversalCounts,
) -> bool {
    // Counted locally (in registers) and added to `total` at the end or
    // handed to the mutator log before a park.
    let mut counts = TraversalCounts::default();
    let mut work = WorkStack::new();
    let mut slots: Option<Vec<*mut *mut u8>> = None;
    let root_address = root.payload().as_ptr() as usize;
    let mut card_owner = mark(root, &mut counts);
    work.push(root_address);
    let mut next_poll = PARK_POLL_OBJECTS;
    while let Some(address) = work.pop() {
        let object = HeapObject::from_payload(
            GcPayload::from_raw(address as *mut u8).expect("queued payload is non-null"),
        );
        let layout = object.with_live_layout(|layout, _| *layout);
        let type_id = layout.type_id as u32;
        let payload_words =
            (layout.size as usize - std::mem::size_of::<GcHeader>()) / GC_STORAGE_WORD_BYTES;
        if type_id == willow_abi::GC_BITMAP_TYPE_ID {
            // Word 0 of the bitmap holds the inline slots, words 1.. the rest.
            let descriptor = layout.gc_ref_mask as *const u64;
            // SAFETY: a bitmap object's mask is its immutable, process-lifetime
            // descriptor: a word count, then that many bitmap words.
            let count = unsafe { *descriptor } as usize;
            for word in 0..count {
                // SAFETY: `word + 1` is below the declared count plus one.
                let mut bits = unsafe { *descriptor.add(word + 1) };
                while bits != 0 {
                    let index = word * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if index < payload_words {
                        counts.slots += 1;
                        card_owner |=
                            visit_slot(object.payload_slot(index), hooks, &mut work, &mut counts);
                    }
                }
            }
        } else {
            let mut mask = layout.gc_ref_mask
                & u64::MAX
                    .checked_shr(64 - payload_words.min(64) as u32)
                    .unwrap_or(0);
            // Counted per slot: `count_ones` is a software popcount on the
            // baseline x86-64 target.
            while mask != 0 {
                let index = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                counts.slots += 1;
                card_owner |= visit_slot(object.payload_slot(index), hooks, &mut work, &mut counts);
            }
        }
        // Only a runtime trace hook needs the slot buffer; the built-in
        // array's contiguous slots are visited in place, as by a minor.
        if let Some(trace) = hooks.get(type_id) {
            // SAFETY: The publishing task owns this live buffer. visit_slot loads
            // its initialized references atomically. A registered mutator can
            // next park only after the entire range is consumed below. Unregistered
            // publication holds the heap lock across traversal, preventing relocation.
            if let Some((first, len)) = unsafe {
                crate::array::minor_reference_range(type_id, Some(trace), object.payload().as_ptr())
            } {
                counts.slots += len as u64;
                for index in 0..len {
                    // SAFETY: `index` is below the array's length.
                    let slot = unsafe { first.add(index) };
                    card_owner |= visit_slot(slot, hooks, &mut work, &mut counts);
                }
            } else {
                let slots =
                    slots.get_or_insert_with(|| SLOTS.try_with(Cell::take).unwrap_or_default());
                slots.clear();
                // SAFETY: the hook is registered for this runtime type and only
                // exposes the object's reference slots, allocating no GC object.
                unsafe { trace(object.payload().as_ptr(), slots) };
                counts.slots += slots.len() as u64;
                for &slot in slots.iter() {
                    card_owner |= visit_slot(slot, hooks, &mut work, &mut counts);
                }
            }
        }
        if !matches!(park_mode, Park::Disabled) && counts.marked >= next_poll {
            next_poll = counts.marked + PARK_POLL_OBJECTS;
            if owner.gc.stop_requested.load(Ordering::Acquire) && !work.is_empty() {
                // The minor of this stop accounts every published young
                // object, so hand over the counts gathered so far first.
                if let Some(log) = mutator_log::bound_in(&owner.gc) {
                    // SAFETY: the bound thread owns its log while running.
                    let logged = &mut unsafe { log.publication() }.counts;
                    logged.marked +=
                        std::mem::take(&mut counts.marked) + std::mem::take(&mut total.marked);
                    logged.slots +=
                        std::mem::take(&mut counts.slots) + std::mem::take(&mut total.slots);
                    logged.promoted_objects += std::mem::take(&mut counts.promoted_objects)
                        + std::mem::take(&mut total.promoted_objects);
                    logged.promoted_bytes += std::mem::take(&mut counts.promoted_bytes)
                        + std::mem::take(&mut total.promoted_bytes);
                    next_poll = PARK_POLL_OBJECTS;
                }
                let pending = park(owner, root_address, work.take_all(), &mut park_mode);
                work.restore(pending);
            }
        }
    }
    work.finish();
    if let Some(mut slots) = slots {
        slots.clear();
        let _ = SLOTS.try_with(|cell| cell.set(slots));
    }
    total.marked += counts.marked;
    total.slots += counts.slots;
    total.promoted_objects += counts.promoted_objects;
    total.promoted_bytes += counts.promoted_bytes;
    card_owner
}

/// Traversal stack: a small inline array (published messages are mostly a
/// few objects) spilling into this thread's reusable buffer, so a small
/// publication touches no thread-local buffer at all.
struct WorkStack {
    inline: [std::mem::MaybeUninit<usize>; WORK_INLINE],
    len: usize,
    spill: Vec<usize>,
}

const WORK_INLINE: usize = 16;

impl WorkStack {
    #[inline(always)]
    fn new() -> Self {
        Self {
            inline: [std::mem::MaybeUninit::uninit(); WORK_INLINE],
            len: 0,
            spill: Vec::new(),
        }
    }

    #[inline(always)]
    fn push(&mut self, address: usize) {
        if self.len < WORK_INLINE {
            self.inline[self.len].write(address);
            self.len += 1;
        } else if self.spill.capacity() != 0 {
            self.spill.push(address);
        } else {
            self.push_spill(address);
        }
    }

    /// First spill of this traversal: take this thread's buffer.
    #[cold]
    #[inline(never)]
    fn push_spill(&mut self, address: usize) {
        self.spill = WORK.try_with(Cell::take).unwrap_or_default();
        self.spill.clear();
        self.spill.push(address);
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<usize> {
        if let Some(address) = self.spill.pop() {
            return Some(address);
        }
        self.len = self.len.checked_sub(1)?;
        // SAFETY: entries below `len` were written by `push`.
        Some(unsafe { self.inline[self.len].assume_init() })
    }

    fn is_empty(&self) -> bool {
        self.len == 0 && self.spill.is_empty()
    }

    /// Every queued address, for a park.
    fn take_all(&mut self) -> Vec<usize> {
        let mut all = std::mem::take(&mut self.spill);
        // SAFETY: entries below `len` were written by `push`.
        all.extend(
            self.inline[..self.len]
                .iter()
                .map(|entry| unsafe { entry.assume_init() }),
        );
        self.len = 0;
        all
    }

    fn restore(&mut self, pending: Vec<usize>) {
        self.spill = pending;
    }

    #[inline(always)]
    fn finish(self) {
        if self.spill.capacity() != 0 {
            let mut spill = self.spill;
            spill.clear();
            let _ = WORK.try_with(|cell| cell.set(spill));
        }
    }
}

/// Mark the not yet shared child in `slot`, queueing it when it has slots;
/// returns whether it is an old card-remembered buffer. One descriptor read.
#[inline(always)]
fn visit_slot(
    slot: *mut *mut u8,
    hooks: &mut TraceHooks<'_>,
    work: &mut WorkStack,
    counts: &mut TraversalCounts,
) -> bool {
    // SAFETY: trace metadata names live reference slots of a published
    // object; only its owning task stores to them.
    let child = unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot) }.load(Ordering::Relaxed);
    let Some(child) = GcPayload::from_raw(child) else {
        return false;
    };
    let mut child = HeapObject::from_payload(child);
    if child.shared() {
        return false;
    }
    let (is_frame, mut layout) = child
        .with_live_layout(|layout, is_static| (hooks.frames.is_frame(layout, is_static), *layout));
    if is_frame {
        // A task handle: the frame stays private; a handed-over result is
        // part of the published graph now.
        let Some(result) =
            escape_frame(child).and_then(|result| past_frames(result, &mut hooks.frames))
        else {
            return false;
        };
        child = result;
        layout = child.with_live_layout(|layout, _| *layout);
    }
    let type_id = layout.type_id as u32;
    counts.marked += 1;
    let card_owner = if child.generation() == GC_GENERATION_OLD {
        child.set_shared(false);
        remembered::card_array_capacity(child).is_some()
    } else {
        child.set_shared(true);
        counts.promoted_objects += 1;
        counts.promoted_bytes += layout.size;
        false
    };
    let has_slots = type_id == willow_abi::GC_BITMAP_TYPE_ID
        || layout.gc_ref_mask != 0
        || hooks.may_have(type_id);
    if has_slots {
        work.push(child.payload().as_ptr() as usize);
    }
    card_owner
}

/// A publication parked at a stop.
pub(super) struct Parked {
    pub(super) root: usize,
    pub(super) pending: Vec<usize>,
}

/// Park at a requested stop with `pending` shared objects not yet scanned.
/// The minor of this stop retains `root` and `pending` in place (the
/// traversal holds their addresses); their young children move with updated
/// slots, which the traversal then reads.
#[cold]
fn park(
    owner: &crate::runtime::Runtime,
    root: usize,
    pending: Vec<usize>,
    mode: &mut Park<'_>,
) -> Vec<usize> {
    let gc = &*owner.gc;
    let me = std::thread::current().id();
    gc.publication
        .parked
        .lock()
        .unwrap()
        .insert(me, Parked { root, pending });
    gc.publication.parks.fetch_add(1, Ordering::Relaxed);
    if let Park::WithSendOperands(operands) = mode {
        let channel = GcRootHandle::new(operands[0]);
        let message = GcRootHandle::new(operands[1]);
        safepoint::safepoint_in(owner);
        **operands = [channel.load(), message.load()];
    } else {
        safepoint::safepoint_in(owner);
    }
    gc.publication
        .parked
        .lock()
        .unwrap()
        .remove(&me)
        .map(|parked| parked.pending)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
