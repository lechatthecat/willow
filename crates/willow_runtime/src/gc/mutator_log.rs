//! Mutator-private logs for old-to-young edges and runtime root-slot
//! registrations.
//!
//! The write barrier's generational record and root-slot add/remove used to
//! take shared locks on every call: the heap mutex for the remembered set and
//! a root-set shard mutex for each registration. A registered mutator instead
//! appends to its own bounded log without any lock:
//!
//! * Only the bound thread appends, and it never reaches a safepoint while
//!   appending (barriers and root operations contain no GC point), so the log
//!   needs no synchronization with its writer.
//! * A collector reads another mutator's log only inside a stop, after the
//!   owner parked under the coordination lock. Idle mutators are forbidden to
//!   touch logs. [`drain_stopped`] runs at the start of every stop, so every
//!   collection, root snapshot, remark and sweep handoff sees all edges and
//!   registrations logged before it.
//! * A full log, unregistration and diagnostic readers flush the caller's own
//!   log under the same locks the old path took. That slow path runs at most
//!   once per [`EDGE_LOG_CAPACITY`] or [`ROOT_LOG_CAPACITY`] entries.
//!
//! Entries resolved later stay exact. Generations change only inside stops,
//! young objects are freed only by minors (which drain first), and an owner
//! logged after a stop is reachable then, so the sweep that follows the next
//! remark cannot reclaim it before that remark drains the entry.
//!
//! Root registrations are signed deltas: a handle created on one worker can be
//! dropped on another after its task migrates. The shared set keeps signed
//! counts, so the two halves may be applied in either order.
//!
//! A log is an owned object bound to a thread by [`bind`]. Rebinding it to a
//! task instead of a thread needs only a different binding; the drain and
//! flush protocol is unchanged.
use super::*;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::sync::atomic::AtomicIsize;

/// Edge entries a mutator may hold before flushing under the heap lock. The
/// stop applies at most this many entries per mutator, each touching a cold
/// owner header, so this bounds the work the log adds to a pause. A full log
/// is applied by its writer while the owners are still in cache.
pub(super) const EDGE_LOG_CAPACITY: usize = 16;
/// Recent edge entries compared with a new one for an exact duplicate.
const EDGE_DEDUP_WINDOW: usize = 8;
/// Root-slot deltas a mutator may hold before flushing to the shared set.
pub(super) const ROOT_LOG_CAPACITY: usize = 128;
/// Recent root entries searched for an opposite delta to cancel. Handles are
/// usually dropped soon after creation, in either LIFO or FIFO order.
const ROOT_CANCEL_WINDOW: usize = 8;
/// Task switches that moved this thread's allocation cursor, held before
/// flushing. A switch that allocated nothing overwrites the last entry.
pub(super) const OWNER_LOG_CAPACITY: usize = 64;
/// TLAB chunks this mutator installed since the last stop, remembered as a
/// lock-free young-address proof for header-unproven stored values.
const YOUNG_CHUNKS: usize = 4;

/// Bases of the TLAB chunks this mutator installed since the last stop.
///
/// Such a chunk holds only young objects: generations change only inside
/// stops, a nursery chunk is installed young, and its storage is reclaimed or
/// reused only by a collection, which runs inside a stop that clears these
/// entries first ([`drain_stopped`]). An address inside one of them is
/// therefore proven young without reading a header or taking the heap lock.
/// The proof only decides whether an owner may be claimed at record time; an
/// unproven value is logged exactly instead, so a missing entry never loses an
/// edge, and a stale one could only remember an owner conservatively.
#[derive(Clone, Copy, Default)]
struct YoungChunks {
    bases: [usize; YOUNG_CHUNKS],
    next: usize,
}

impl YoungChunks {
    fn note(&mut self, base: usize) {
        self.bases[self.next] = base;
        self.next = (self.next + 1) % YOUNG_CHUNKS;
    }

    /// O(`YOUNG_CHUNKS`) unsigned range checks, no memory access beyond the log.
    fn contains(&self, address: usize) -> bool {
        self.bases
            .iter()
            .any(|&base| base != 0 && address.wrapping_sub(base) < GC_TLAB_CHUNK_SIZE)
    }
}

/// Entries of the per-mutator cache of values proven outside every TLAB chunk.
const OLD_VALUES: usize = 16;

/// Chunk epoch of one runtime (`GcRuntime::chunk_epoch`). It gets a cache line
/// of its own: every TLAB install advances it, and a neighbouring hot field
/// would otherwise bounce with each install.
#[repr(align(128))]
pub(super) struct ChunkEpoch(AtomicU64);

impl ChunkEpoch {
    pub(super) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
}

impl std::ops::Deref for ChunkEpoch {
    type Target = AtomicU64;

    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}

/// Values this mutator's own log flushes found outside every TLAB chunk, valid
/// while the runtime's chunk epoch is unchanged.
///
/// Only a TLAB chunk holds young objects, and a chunk joins the heap only
/// through a TLAB install or a survivor destination, both of which advance the
/// epoch under the heap lock. An address outside every chunk at an epoch
/// therefore stays not young until the epoch moves, so a later header-unproven
/// store of the same value adds no edge and is dropped without a log entry.
/// This bounds repeated stores of one old value (a shared object stored into
/// many owners) to one exact entry per epoch, as a claimed owner byte bounds
/// stores of young values.
#[derive(Clone, Copy, Default)]
struct OldValues {
    epoch: u64,
    values: [usize; OLD_VALUES],
}

impl OldValues {
    fn index(value: usize) -> usize {
        // Payload starts are 8-aligned; fold higher bits in so objects of one
        // size class do not share a few entries.
        ((value >> 3) ^ (value >> 9)) % OLD_VALUES
    }

    /// One thread-local compare; the shared epoch is read only on a slot hit,
    /// so a miss touches no cache line other mutators write.
    fn contains(&self, value: usize, epoch: &AtomicU64) -> bool {
        value != 0
            && self.values[Self::index(value)] == value
            && self.epoch == epoch.load(Ordering::Acquire)
    }

    fn note(&mut self, value: usize, epoch: u64) {
        if self.epoch != epoch {
            *self = Self {
                epoch,
                ..Self::default()
            };
        }
        self.values[Self::index(value)] = value;
    }
}

/// Publication counters of one mutator (`gc::publish`), merged into the
/// runtime's shared counters when the log is applied, so a publication or a
/// republication touches no contended cache line.
#[derive(Clone, Copy, Default)]
pub(super) struct PublicationCounts {
    pub(super) calls: u64,
    pub(super) republished: u64,
    pub(super) handed_over: u64,
    pub(super) marked: u64,
    pub(super) slots: u64,
    pub(super) promoted_objects: u64,
    pub(super) promoted_bytes: u64,
    pub(super) per_point: [u64; 8],
}

impl PublicationCounts {
    fn is_empty(&self) -> bool {
        self.calls == 0 && self.republished == 0 && self.handed_over == 0 && self.marked == 0
    }
}

/// Publication counters and allocation-owner changes of one mutator
/// (`gc::publish`). Applied with the edge log: at every stop, when full, and
/// when the thread installs a new TLAB chunk (owner entries name only the
/// current chunk).
#[derive(Default)]
pub(super) struct PublicationLog {
    pub(super) counts: PublicationCounts,
    /// Base of the chunk the owner entries refer to.
    chunk_base: usize,
    /// `(cursor, owner)`: bytes allocated from `cursor` on belong to `owner`.
    owners: Vec<(usize, u64)>,
}

impl PublicationLog {
    fn is_empty(&self) -> bool {
        self.counts.is_empty() && self.owners.is_empty()
    }

    pub(super) fn note_owner(&mut self, cursor: usize, owner: u64) -> bool {
        match self.owners.last_mut() {
            Some(last) if last.0 == cursor => {
                last.1 = owner;
                false
            }
            _ => {
                self.owners.push((cursor, owner));
                self.owners.len() == OWNER_LOG_CAPACITY
            }
        }
    }

    /// Apply under the heap lock or in a stop. O(owner entries).
    pub(super) fn apply(&mut self, state: &mut GcState, publication: &super::publish::Publication) {
        let counts = std::mem::take(&mut self.counts);
        if !counts.is_empty() {
            // Published young objects stay young; each minor accounts the
            // ones that died young (`minor`).
            state.published_young_bytes = state
                .published_young_bytes
                .saturating_add(counts.promoted_bytes);
            publication.merge(&counts);
        }
        if !self.owners.is_empty() {
            if let Some(index) = state.tlab_addresses.exact(self.chunk_base) {
                let capacity = state.tlab_chunks[index].capacity;
                let base = self.chunk_base;
                let segments = state.chunk_owners.entry(base).or_default();
                for &(cursor, owner) in &self.owners {
                    if let Some(offset) = cursor.checked_sub(base) {
                        segments.mark_owner_from(offset, capacity, owner);
                    }
                }
            }
            self.owners.clear();
        }
    }

    /// The thread's TLAB moves to the chunk at `base`; entries for the
    /// previous chunk were applied by the caller.
    pub(super) fn start_chunk(&mut self, base: usize) {
        debug_assert!(self.owners.is_empty());
        self.chunk_base = base;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    /// A header-proven old owner of a proven young value: its header says it
    /// is not old, or it lies in a chunk this mutator installed since the last
    /// stop ([`YoungChunks`]). The logging thread already claimed the owner's
    /// remembered byte, which deduplicates the owner across all threads until
    /// the next minor. A store whose value is not proven young is logged as
    /// `Unresolved` instead, so an old value never remembers its owner.
    Owner(usize),
    /// A header-proven old card-remembered buffer and the card of a store whose
    /// value may be young (remembered conservatively like `Owner`).
    Card { owner: usize, card: usize },
    /// A store whose owner is not header-proven. Classified under the
    /// heap lock when the log is applied, exactly as the locked barrier did.
    Unresolved {
        owner: usize,
        slot: usize,
        value: usize,
        kind: i64,
    },
}

pub(super) struct MutatorLog {
    edges: UnsafeCell<Vec<Edge>>,
    young_chunks: UnsafeCell<YoungChunks>,
    old_values: UnsafeCell<OldValues>,
    roots: UnsafeCell<Vec<(usize, isize)>>,
    publication: UnsafeCell<PublicationLog>,
    /// Net registrations held in `roots`. Stored only by whoever owns the log
    /// at that moment; read by the registration count diagnostic.
    root_net: AtomicIsize,
}

// SAFETY: `edges` and `roots` are accessed by the bound thread while it runs,
// or by a collector while that thread is parked or unbound. The coordination
// lock orders those accesses (see the module documentation).
unsafe impl Sync for MutatorLog {}

impl MutatorLog {
    /// Fixed storage of one log, charged to the memory budget while bound.
    pub(super) const BYTES: usize = EDGE_LOG_CAPACITY * std::mem::size_of::<Edge>()
        + ROOT_LOG_CAPACITY * std::mem::size_of::<(usize, isize)>()
        + OWNER_LOG_CAPACITY * std::mem::size_of::<(usize, u64)>();

    pub(super) fn new() -> Self {
        Self {
            edges: UnsafeCell::new(Vec::with_capacity(EDGE_LOG_CAPACITY)),
            young_chunks: UnsafeCell::new(YoungChunks::default()),
            old_values: UnsafeCell::new(OldValues::default()),
            roots: UnsafeCell::new(Vec::with_capacity(ROOT_LOG_CAPACITY)),
            publication: UnsafeCell::new(PublicationLog {
                owners: Vec::with_capacity(OWNER_LOG_CAPACITY),
                ..PublicationLog::default()
            }),
            root_net: AtomicIsize::new(0),
        }
    }

    /// # Safety
    /// The caller has exclusive access (see the `Sync` impl).
    #[allow(
        clippy::mut_from_ref,
        reason = "only the bound owner thread, or a stopper while it is parked, reaches the log"
    )]
    unsafe fn edges(&self) -> &mut Vec<Edge> {
        unsafe { &mut *self.edges.get() }
    }

    /// # Safety
    /// As for [`Self::edges`].
    #[allow(
        clippy::mut_from_ref,
        reason = "only the bound owner thread, or a stopper while it is parked, reaches the log"
    )]
    unsafe fn roots(&self) -> &mut Vec<(usize, isize)> {
        unsafe { &mut *self.roots.get() }
    }

    /// # Safety
    /// As for [`Self::edges`].
    #[allow(
        clippy::mut_from_ref,
        reason = "only the bound owner thread, or a stopper while it is parked, reaches the log"
    )]
    pub(super) unsafe fn publication(&self) -> &mut PublicationLog {
        unsafe { &mut *self.publication.get() }
    }

    /// # Safety
    /// As for [`Self::edges`].
    #[allow(
        clippy::mut_from_ref,
        reason = "only the bound owner thread, or a stopper while it is parked, reaches the log"
    )]
    unsafe fn young_chunks(&self) -> &mut YoungChunks {
        unsafe { &mut *self.young_chunks.get() }
    }

    /// # Safety
    /// As for [`Self::edges`].
    #[allow(
        clippy::mut_from_ref,
        reason = "only the bound owner thread, or a stopper while it is parked, reaches the log"
    )]
    unsafe fn old_values(&self) -> &mut OldValues {
        unsafe { &mut *self.old_values.get() }
    }

    /// Record the TLAB chunk at `base` that this mutator just installed.
    ///
    /// # Safety
    /// As for [`Self::edges`].
    pub(super) unsafe fn note_young_chunk(&self, base: usize) {
        unsafe { self.young_chunks() }.note(base);
    }

    pub(super) fn pending_root_net(&self) -> isize {
        self.root_net.load(Ordering::Relaxed)
    }

    /// # Safety
    /// As for [`Self::edges`].
    unsafe fn is_empty(&self) -> bool {
        unsafe {
            self.edges().is_empty() && self.roots().is_empty() && self.publication().is_empty()
        }
    }
}

thread_local! {
    /// The log of this thread's current mutator registration, or null.
    static BOUND: Cell<*const MutatorLog> = const { Cell::new(std::ptr::null()) };
    /// Keeps `BOUND` dereferenceable even if a reset discards the registry.
    static KEEP: RefCell<Option<Arc<MutatorLog>>> = const { RefCell::new(None) };
}

/// Bind `log` to this thread. Called by registration under the coordination
/// lock, before the thread can store or register roots as a mutator.
pub(super) fn bind(log: Arc<MutatorLog>) {
    BOUND.set(Arc::as_ptr(&log));
    KEEP.with(|keep| *keep.borrow_mut() = Some(log));
}

/// Unbind this thread's log. Its entries must already be flushed.
pub(super) fn unbind() {
    BOUND.set(std::ptr::null());
    KEEP.with(|keep| keep.borrow_mut().take());
}

/// This thread's log when it is a registered mutator of `gc`. Not inlined so a
/// caller that later migrates never reuses another thread's TLS address.
#[inline(never)]
pub(super) fn bound_in(gc: &GcRuntime) -> Option<&MutatorLog> {
    if !safepoint::current_thread_is_registered_in(gc) {
        return None;
    }
    let log = BOUND.get();
    // SAFETY: non-null while `KEEP` owns the log; the registration check ties
    // it to `gc`, so it outlives this borrow of `gc`.
    (!log.is_null()).then(|| unsafe { &*log })
}

/// Whether this thread already holds a log for `gc` (re-registration keeps it).
pub(super) fn is_bound_in(gc: &GcRuntime) -> bool {
    bound_in(gc).is_some()
}

/// Owner lookups of proven payload starts: any non-static, non-interior kind.
const OBJECT_KIND: i64 = GcStoreDestination::ObjectField as i64;

fn remembered_byte(owner: usize, remembered: bool) {
    if let Some(payload) = GcPayload::from_raw(owner as *mut u8) {
        HeapObject::from_payload(payload).set_remembered(remembered);
    }
}

fn try_claim_remembered(owner: usize) -> bool {
    let Some(payload) = GcPayload::from_raw(owner as *mut u8) else {
        return false;
    };
    HeapObject::from_payload(payload).try_claim_remembered()
}

/// Log the generational half of a barrier whose young edge is possible.
/// Returns false when this thread has no log for `owner`'s runtime; the caller
/// then takes the locked path.
///
/// `owner_proven` means the inline-filter facts hold for `owner`: it is an
/// old, unremembered payload start and `slot` is not an interior reference.
/// `value_proven` means the value's header proved it is not old.
#[allow(
    clippy::too_many_arguments,
    reason = "the barrier's store facts are passed through unpacked on the store path"
)]
pub(super) fn record_edge_in(
    runtime_owner: &crate::runtime::Runtime,
    owner: *mut u8,
    slot: *mut u8,
    value: *mut u8,
    kind: i64,
    owner_proven: bool,
    value_proven: bool,
    serves_thread: bool,
) -> bool {
    let gc = &*runtime_owner.gc;
    let Some(log) = bound_in(gc) else {
        return false;
    };
    // A value one of this log's flushes found outside every TLAB chunk, with
    // no chunk added since, is still not young: the store adds no edge.
    // SAFETY: the bound thread owns its log while running.
    if !value_proven && unsafe { log.old_values() }.contains(value as usize, &gc.chunk_epoch) {
        count_old_value_drop();
        return true;
    }
    let card = (owner_proven && !slot.is_null())
        .then(|| barrier::slot_card(owner as usize, slot as usize))
        .flatten();
    let edge = match card {
        Some(card) => {
            if serves_thread {
                // The entry reaches the set before the next minor advances
                // the epoch, so this proof holds for the cached epoch.
                barrier::card_cache_fill(owner as usize, card);
            }
            Edge::Card {
                owner: owner as usize,
                card,
            }
        }
        // Claim only for a value proven young. Without that proof the value
        // may be old (no generation is readable lock-free), and a claim would
        // keep the owner remembered until the next minor rescans it.
        // SAFETY: the bound thread owns its log while running.
        None if owner_proven
            && (value_proven || unsafe { log.young_chunks() }.contains(value as usize)) =>
        {
            count_owner_claim(value_proven);
            if !try_claim_remembered(owner as usize) {
                // Another thread logged this owner; its entry covers ours.
                return true;
            }
            Edge::Owner(owner as usize)
        }
        None => Edge::Unresolved {
            owner: owner as usize,
            slot: slot as usize,
            value: value as usize,
            kind,
        },
    };
    // SAFETY: the bound thread owns its log while running.
    let edges = unsafe { log.edges() };
    // An identical recent entry already records this edge; a store repeating
    // an (owner, slot, value) triple adds nothing the collector needs.
    if edges
        .iter()
        .rev()
        .take(EDGE_DEDUP_WINDOW)
        .any(|logged| *logged == edge)
    {
        return true;
    }
    if edges.len() == EDGE_LOG_CAPACITY {
        // SAFETY: as above; a different cell from `edges`.
        flush_edges(
            runtime_owner,
            edges,
            unsafe { log.old_values() },
            serves_thread,
        );
    }
    edges.push(edge);
    true
}

/// Log a root-slot registration delta. Returns false when this thread has no
/// log for `owner`; the caller then updates the shared set directly.
pub(super) fn record_root_in(owner: &crate::runtime::Runtime, slot: usize, delta: isize) -> bool {
    let Some(log) = bound_in(&owner.gc) else {
        return false;
    };
    // SAFETY: the bound thread owns its log while running.
    let roots = unsafe { log.roots() };
    let window = roots.len().saturating_sub(ROOT_CANCEL_WINDOW);
    if let Some(index) = roots[window..]
        .iter()
        .rposition(|&(logged, logged_delta)| logged == slot && logged_delta == -delta)
    {
        // Signed deltas commute, so cancelling out of order is exact.
        roots.swap_remove(window + index);
    } else {
        if roots.len() == ROOT_LOG_CAPACITY {
            flush_roots(&owner.gc, log, roots);
        }
        roots.push((slot, delta));
    }
    log.root_net
        .store(log.pending_root_net() + delta, Ordering::Relaxed);
    true
}

/// Apply edge entries to the remembered set. `card_proof` receives each card
/// a resolved entry proved recorded, for a caller filling its own card cache;
/// `outside_chunks` each unresolved value found outside every TLAB chunk.
fn apply_edges(
    state: &mut GcState,
    edges: &mut Vec<Edge>,
    mut card_proof: impl FnMut(usize, usize),
    mut outside_chunks: impl FnMut(usize),
) {
    let mut new_entries = 0u64;
    for edge in edges.drain(..) {
        let added = match edge {
            // Header facts prove the generations, not that the owner lives in
            // this heap; check that exactly as the locked barrier does. A
            // rejected owner had its byte claimed only by entries for the
            // same owner, which are all rejected too, so clearing loses none.
            Edge::Owner(owner) => {
                if barrier::barrier_owner_payload(state, owner as *mut u8, OBJECT_KIND)
                    != Some(owner)
                {
                    remembered_byte(owner, false);
                    continue;
                }
                barrier::remember_owner(state, owner)
            }
            Edge::Card { owner, card } => {
                if barrier::barrier_owner_payload(state, owner as *mut u8, OBJECT_KIND)
                    != Some(owner)
                {
                    continue;
                }
                barrier::remember_card(state, owner, card)
            }
            Edge::Unresolved {
                owner,
                slot,
                value,
                kind,
            } => {
                match barrier::tlab_chunk_generation(state, value) {
                    Some(Some(GC_GENERATION_YOUNG)) => {}
                    Some(_) => continue,
                    None => {
                        outside_chunks(value);
                        continue;
                    }
                }
                let Some(payload) = barrier::barrier_owner_payload(state, owner as *mut u8, kind)
                else {
                    continue;
                };
                let added = barrier::remember_store(state, payload, slot as *mut u8);
                if payload == owner
                    && slot != 0
                    && let Some(card) = barrier::slot_card(payload, slot)
                    && state
                        .remembered_set
                        .owner_cards(payload)
                        .is_some_and(|cards| cards.contains(&card))
                {
                    card_proof(payload, card);
                }
                added
            }
        };
        new_entries += u64::from(added);
    }
    state.write_barrier_hits = state.write_barrier_hits.saturating_add(new_entries);
}

fn apply_roots(gc: &GcRuntime, log: &MutatorLog, roots: &mut Vec<(usize, isize)>) {
    // Each delta leaves the log's net inside the shard update that applies
    // it, so `runtime_root_count_in` never counts it twice or not at all.
    // Only this applier writes `root_net` meanwhile (the owner, or a stopper
    // while the owner is parked).
    for (slot, delta) in roots.drain(..) {
        gc.runtime_root_slots
            .adjust_then(slot as *mut u8, delta, || {
                log.root_net
                    .store(log.pending_root_net() - delta, Ordering::Relaxed);
            });
    }
}

/// Slow path of a full edge log: apply it under the heap lock. Takes the
/// caller's one borrow of the log's edges, so no second `&mut` is created.
#[cold]
fn flush_edges(
    runtime_owner: &crate::runtime::Runtime,
    edges: &mut Vec<Edge>,
    old_values: &mut OldValues,
    serves_thread: bool,
) {
    let _interval = crate::gc_telemetry::observations::Span::new_in(
        &runtime_owner.telemetry.observations,
        crate::gc_telemetry::observations::Kind::BarrierSlow,
    );
    // Diagnostic readers flush here and must keep reporting after a panic
    // poisoned the heap lock, as their own reads do.
    let mut state = runtime_owner
        .gc
        .heap
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    count_shared_lock();
    // Chunks join the heap only under this lock, so the epoch read here
    // matches every classification below.
    let epoch = runtime_owner.gc.chunk_epoch.load(Ordering::Relaxed);
    apply_edges(
        &mut state,
        edges,
        |owner, card| {
            if serves_thread {
                barrier::card_cache_fill(owner, card);
            }
        },
        |value| old_values.note(value, epoch),
    );
}

/// Slow path of a full root log: apply it to the shared shards. Takes the
/// caller's one borrow of `log`'s roots, so no second `&mut` is created.
#[cold]
fn flush_roots(gc: &GcRuntime, log: &MutatorLog, roots: &mut Vec<(usize, isize)>) {
    count_shared_lock();
    apply_roots(gc, log, roots);
}

/// Flush this thread's log for `owner`, if it has one. Unregistration and
/// diagnostic readers call this before reading or leaving the shared state.
/// Must not be called with the heap lock held.
pub(super) fn flush_current_in(owner: &crate::runtime::Runtime) {
    let Some(log) = bound_in(&owner.gc) else {
        return;
    };
    // SAFETY: the bound thread owns its log while running; each vector is
    // borrowed once.
    let edges = unsafe { log.edges() };
    if !edges.is_empty() {
        let serves_thread = std::ptr::eq(owner, crate::runtime::current());
        // SAFETY: as above; a different cell from `edges`.
        flush_edges(owner, edges, unsafe { log.old_values() }, serves_thread);
    }
    let roots = unsafe { log.roots() };
    if !roots.is_empty() {
        flush_roots(&owner.gc, log, roots);
    }
    let publication = unsafe { log.publication() };
    if !publication.is_empty() {
        flush_publication(owner, publication);
    }
}

/// Slow path of a full publication log: apply it under the heap lock.
#[cold]
pub(super) fn flush_publication(owner: &crate::runtime::Runtime, publication: &mut PublicationLog) {
    let mut state = owner
        .gc
        .heap
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    count_shared_lock();
    publication.apply(&mut state, &owner.gc.publication);
}

/// Apply every registered mutator's log. Called at the start of each stop,
/// with the coordination lock held and every other registered mutator parked
/// or idle, before any collector reads the remembered set or root slots.
pub(super) fn drain_stopped(owner: &crate::runtime::Runtime, coord: &GcCoord) {
    let me = std::thread::current().id();
    let logs = || {
        coord
            .mutator_logs
            .iter()
            .filter(|(id, _)| **id == me || coord.mutators.contains_key(*id))
            .map(|(_, log)| log)
    };
    // SAFETY (all log accesses below): owners are parked, idle or this thread.
    // This stop may change generations or reuse chunk storage, so no chunk
    // installed before it proves an address young any more.
    for log in logs() {
        *unsafe { log.young_chunks() } = YoungChunks::default();
    }
    if logs().all(|log| unsafe { log.is_empty() }) {
        return;
    }
    let _component = crate::gc_telemetry::pause::Span::new(
        &owner.telemetry.pause,
        crate::gc_telemetry::pause::Component::RememberedLog,
    );
    let mut entries = 0u64;
    {
        let mut state = owner.gc.heap.lock().unwrap();
        for log in logs() {
            let edges = unsafe { log.edges() };
            entries += edges.len() as u64;
            // Another thread's card cache is not ours to fill.
            // A collection may follow in this stop and add survivor chunks
            // after the drain, so nothing is cached here.
            apply_edges(&mut state, edges, |_, _| {}, |_| {});
            let publication = unsafe { log.publication() };
            entries += publication.owners.len() as u64;
            publication.apply(&mut state, &owner.gc.publication);
        }
    }
    for log in logs() {
        let roots = unsafe { log.roots() };
        entries += roots.len() as u64;
        apply_roots(&owner.gc, log, roots);
    }
    owner
        .gc
        .mutator_log_drained_entries
        .fetch_add(entries, Ordering::Relaxed);
}

/// Net root-slot registrations: the shared shards plus every log's pending
/// deltas. The coordination lock keeps the log set fixed (taken before the
/// shards, as the stop drain does), and holding every shard lock excludes a
/// concurrent flush, so no applied delta is counted twice or missed.
///
/// Running mutators still append to their logs without either lock, and a
/// task can add a slot on one worker and remove it on another. The pending
/// part is therefore an in-flight approximation: a read can miss an add whose
/// matching remove it sees, so the sum can briefly be below the true count,
/// even negative. It is exact once every log is quiescent.
pub(super) fn net_root_slots(gc: &GcRuntime) -> isize {
    let coord = gc.coord.0.lock().unwrap_or_else(|p| p.into_inner());
    gc.runtime_root_slots.net_with(|| {
        coord
            .mutator_logs
            .values()
            .map(|log| {
                let net = log.pending_root_net();
                #[cfg(test)]
                after_log_read_for_test(&**log as *const MutatorLog as usize);
                net
            })
            .sum()
    })
}

/// Test hook run on the reading thread after each log's pending net is read
/// (with the log's address), to place a concurrent append between two reads.
#[cfg(test)]
type LogReadHook = Box<dyn FnMut(usize)>;

#[cfg(test)]
thread_local! {
    static AFTER_LOG_READ: RefCell<Option<LogReadHook>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn after_log_read_for_test(log: usize) {
    AFTER_LOG_READ.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(log);
        }
    });
}

#[cfg(test)]
pub(super) fn set_after_log_read_for_test(hook: Option<LogReadHook>) {
    AFTER_LOG_READ.with(|slot| *slot.borrow_mut() = hook);
}

/// Address of this thread's bound log for `gc` (test probe).
#[cfg(test)]
pub(super) fn bound_log_address_for_test(gc: &GcRuntime) -> Option<usize> {
    bound_in(gc).map(|log| log as *const MutatorLog as usize)
}

thread_local! {
    /// Shared-lock acquisitions by this thread's log slow paths (test probe).
    #[cfg(test)]
    static SHARED_LOCKS: Cell<u64> = const { Cell::new(0) };
}

#[inline]
pub(super) fn count_shared_lock() {
    #[cfg(test)]
    SHARED_LOCKS.set(SHARED_LOCKS.get() + 1);
}

#[cfg(test)]
pub(super) fn shared_locks_for_test() -> u64 {
    SHARED_LOCKS.get()
}

thread_local! {
    /// Owner claims by this thread, split by the young proof of the value:
    /// `[header, own chunk]` (test probe).
    #[cfg(test)]
    static OWNER_CLAIMS: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
}

#[inline]
fn count_owner_claim(header_proven: bool) {
    #[cfg(test)]
    OWNER_CLAIMS.set({
        let mut claims = OWNER_CLAIMS.get();
        claims[usize::from(!header_proven)] += 1;
        claims
    });
    #[cfg(not(test))]
    let _ = header_proven;
}

#[cfg(test)]
pub(super) fn owner_claims_for_test() -> [u64; 2] {
    OWNER_CLAIMS.get()
}

thread_local! {
    /// Stores dropped by this thread's old-value cache (test probe).
    #[cfg(test)]
    static OLD_VALUE_DROPS: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn count_old_value_drop() {
    #[cfg(test)]
    OLD_VALUE_DROPS.set(OLD_VALUE_DROPS.get() + 1);
}

#[cfg(test)]
pub(super) fn old_value_drops_for_test() -> u64 {
    OLD_VALUE_DROPS.get()
}

/// Cache `value` in this thread's log as outside every chunk at the current
/// epoch, as a flush would (test hook for stale-cache negative checks).
#[cfg(test)]
pub(super) fn note_old_value_for_test(owner: &crate::runtime::Runtime, value: usize) {
    let epoch = owner.gc.chunk_epoch.load(Ordering::Relaxed);
    let log = bound_in(&owner.gc).unwrap();
    unsafe { log.old_values() }.note(value, epoch);
}

#[cfg(test)]
#[path = "mutator_log_tests.rs"]
mod tests;
