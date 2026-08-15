//! Concurrent-mark work queue substrate (willow-6fv.5.6.1).
//!
//! This module owns the *transport* for concurrent old-generation marking: the
//! structure that moves mark work between background mark workers, root-scan
//! jobs, SATB buffer flushes, and Mutator Assist. It deliberately knows nothing
//! about mark semantics — an item is an opaque instruction, and this module only
//! guarantees that instructions are delivered.
//!
//! # Shape
//!
//! ```text
//!            external producers (SATB flush, root jobs, remark)
//!                              |
//!                              v
//!                     +------------------+
//!                     |     injector     |  global FIFO, mutex-guarded
//!                     +------------------+
//!                              ^  |
//!                 flush/publish|  | drain (bounded batches)
//!                              |  v
//!   +--------------------------------------------------------------+
//!   | slot 0        | slot 1        | ...        | slot n-1         |
//!   |  private LIFO |  private LIFO |            |  private LIFO    |  <- hidden
//!   |  shared FIFO  |  shared FIFO  |            |  shared FIFO     |  <- stealable
//!   +--------------------------------------------------------------+
//!            ^  steal (bounded batch, oldest-first) ^
//! ```
//!
//! Each registered consumer owns one slot. The *private* segment is the hot
//! path: the owner pushes and pops it LIFO, which keeps marking depth-first and
//! cache-warm. Nothing steals from a private segment — that is a policy, not a
//! data-race question, because the segment is still mutex-guarded so
//! [`MarkWorkQueue::flush_all_locals`] can force it out at a safepoint. The
//! *shared* segment is what thieves see; the owner publishes into it eagerly
//! once the private segment grows past [`LOCAL_PUBLISH_THRESHOLD`], so a worker
//! that falls behind automatically becomes a donor.
//!
//! # Delivery contract
//!
//! Delivery is **at-least-once**. A published item is never lost, but the same
//! item may be handed out more than once — a consumer that dies or retires with
//! work in hand calls [`MarkWorker::relinquish`], which republishes it. Marking
//! must therefore be idempotent (it is: setting a mark bit twice is a no-op).
//! Duplicates are measured, not merely tolerated: see
//! [`MarkQueueSnapshot::republished`].
//!
//! # Termination accounting
//!
//! Termination detection (willow-6fv.5.6.5) needs one question answered without
//! taking a lock: *is any mark work left anywhere?* Exactly one counter answers
//! it. `outstanding` counts every item that has been accepted and not yet
//! completed — items in the injector, in a shared segment, in an owner-private
//! segment, **and items a consumer is currently holding**. Moving an item
//! between those places does not touch it, so [`MarkQueueSnapshot::is_drained`]
//! is a single atomic load and cannot be torn:
//!
//! * a producer increments it **before** the item becomes reachable, under the
//!   same lock the item is inserted beneath;
//! * a consumer decrements it only when it **completes** an item — the next call
//!   to [`MarkWorker::next_work`], or [`MarkWorker::retire`] — never when it
//!   merely claims one, so there is no window in which an item sits in a
//!   consumer's hand and goes uncounted;
//! * [`MarkWorker::relinquish`] moves an item from a hand back to the injector,
//!   and so leaves the count alone.
//!
//! An earlier version of this module instead required `published`, `hidden`, and
//! `active_consumers` to all read zero. That predicate was unsound: three
//! separate loads are not a snapshot, and a reader could see `published == 0`
//! *before* a publication and `hidden == 0` *after* it, concluding "drained"
//! with the item in the shared segment the whole time. Those three survive as
//! **advisory statistics** — `hidden > 0` is what tells the termination protocol
//! to call [`MarkWorkQueue::flush_all_locals`] — and no correctness decision
//! rests on them. `active_consumers` is diagnostic for the same reason: a
//! consumer with nothing in hand cannot manufacture work, so it does not belong
//! in the predicate.
//!
//! `is_drained` still reports a *moment*: an external producer may inject
//! immediately afterwards. Ending a cycle on it therefore remains the
//! termination protocol's decision, and that protocol quiesces producers first.
//!
//! # Epochs
//!
//! Every item carries the [`MarkEpoch`] it was produced for, and epoch numbers
//! are **never reused**. [`MarkWorkQueue::begin_epoch`] draws from a
//! monotonically advancing counter that [`MarkWorkQueue::end_epoch`] does not
//! reset — ending a cycle parks the *current* epoch at the idle sentinel and
//! nothing more. Reuse would be a correctness hole and not a cosmetic one: a
//! `MarkWork` or a [`MarkWorker`] left over from an earlier cycle would validate
//! as current and feed dead addresses into a live mark.
//!
//! Epoch changes are atomic with respect to producers. `begin_epoch` and
//! `end_epoch` take *every* lock in the order below, so there is no instant at
//! which the epoch has moved on while a segment still holds — or can still
//! accept — work from the old one. Producers validate the epoch under the same
//! lock they insert beneath, which closes the check-then-publish race from the
//! other side, and consumers re-validate on delivery as a backstop.
//!
//! Counters saturate at zero rather than wrapping. A subtraction that would go
//! negative is a bug in this module, so it is `debug_assert!`-ed and, in
//! release builds, recorded in [`MarkQueueSnapshot::counter_underflows`] where a
//! test or a diagnostic can see it instead of silently producing `u64::MAX`.
//!
//! # Safepoints
//!
//! A queue lock is never held across a safepoint wait: every critical section
//! here is a handful of `VecDeque` operations with no callbacks, no allocation
//! hooks, and no blocking. [`assert_no_queue_lock_held`] makes that rule
//! enforceable from the safepoint path rather than merely documented.
//!
//! # Portability
//!
//! Everything here is `std`-only — `Mutex`, `VecDeque`, and atomics — so it
//! behaves identically on Linux, macOS, and Windows. An external work-stealing
//! crate stays out of the runtime until it has been reviewed against all three
//! targets; the API in this module is the seam that would let one be dropped in
//! later without touching callers.

use std::cell::Cell;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Upper bound on the items moved by a single steal or injector drain. Bounded
/// batches keep a thief from emptying a victim (which would just move the
/// imbalance) and keep the victim's lock hold time constant.
pub const MAX_STEAL_BATCH: usize = 32;

/// Private-segment size past which the owner automatically publishes half of
/// its work so peers can steal it.
pub const LOCAL_PUBLISH_THRESHOLD: usize = 64;

/// Most stale items retained for diagnostics. Beyond this they are counted and
/// dropped: quarantine is an inspection aid, not a second queue.
pub const MAX_QUARANTINED_ITEMS: usize = 64;

// ── item types ──────────────────────────────────────────────────────────────

/// A heap object address. Stored as an integer so work items are `Send`/`Sync`
/// without wrapping a raw pointer; the marking engine converts back when it is
/// ready to trace.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjectRef(NonZeroUsize);

impl ObjectRef {
    /// `None` for a null address, which is never a mark target.
    pub fn from_addr(addr: usize) -> Option<Self> {
        NonZeroUsize::new(addr).map(Self)
    }

    pub fn from_ptr<T>(ptr: *mut T) -> Option<Self> {
        Self::from_addr(ptr as usize)
    }

    pub fn addr(self) -> usize {
        self.0.get()
    }

    /// The address as a pointer. Producing it is safe; dereferencing it is the
    /// caller's obligation and requires the object to still be live.
    pub fn as_ptr<T>(self) -> *mut T {
        self.0.get() as *mut T
    }
}

/// Identifies an old-generation region. Opaque here — region bookkeeping lives
/// in [`crate::gc`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RegionId(pub u32);

/// Identifies one shard of the root set, so root scanning can be split across
/// workers instead of serialising behind one thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RootShardId(pub u32);

/// One unit of mark work.
///
/// Kept small (three words) on purpose: the nightly stress target queues
/// hundreds of millions of these, and the enum's size is the per-item cost.
/// Batching therefore goes through a boxed slice rather than an inline array.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MarkWorkItem {
    /// Trace one object.
    Object(ObjectRef),
    /// Trace a group of objects. Produced by SATB buffer flushes, which already
    /// hold a contiguous run, so one queue operation amortises over many objects.
    ObjectBatch(Box<[ObjectRef]>),
    /// Scan one shard of the root set.
    RootShard(RootShardId),
    /// Scan one chunk of a region, for regions too large to be a single unit.
    RegionChunk { region: RegionId, chunk: u32 },
}

impl MarkWorkItem {
    /// Objects this item will make the engine visit. Used for pacing and for
    /// stress assertions; not used for queue accounting, which counts *items*.
    pub fn object_count(&self) -> usize {
        match self {
            MarkWorkItem::Object(_) => 1,
            MarkWorkItem::ObjectBatch(objects) => objects.len(),
            MarkWorkItem::RootShard(_) | MarkWorkItem::RegionChunk { .. } => 0,
        }
    }

    /// An empty batch carries no work; producers should drop it rather than
    /// publish it, and the queue refuses it so termination is not delayed by
    /// items that will never mark anything.
    pub fn is_empty(&self) -> bool {
        match self {
            MarkWorkItem::ObjectBatch(objects) => objects.is_empty(),
            _ => false,
        }
    }
}

/// The mark cycle an item belongs to. `MarkEpoch::IDLE` (0) means "no cycle is
/// running" and is never a valid tag on live work.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MarkEpoch(pub u64);

impl MarkEpoch {
    pub const IDLE: MarkEpoch = MarkEpoch(0);

    pub fn is_idle(self) -> bool {
        self.0 == 0
    }

    /// The next cycle. Epochs are never reused: exhausting the 64-bit counter
    /// is fatal instead of making ancient work appear current again.
    pub fn next(self) -> MarkEpoch {
        MarkEpoch(self.0.checked_add(1).expect("mark epoch exhausted"))
    }
}

/// An epoch-tagged work item. External producers build these explicitly so a
/// producer that was preempted across an epoch boundary hands over work the
/// queue can recognise as stale.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MarkWork {
    pub epoch: MarkEpoch,
    pub item: MarkWorkItem,
}

impl MarkWork {
    pub fn new(epoch: MarkEpoch, item: MarkWorkItem) -> Self {
        Self { epoch, item }
    }

    pub fn object(epoch: MarkEpoch, object: ObjectRef) -> Self {
        Self::new(epoch, MarkWorkItem::Object(object))
    }
}

/// Why [`MarkWorkQueue::inject`] refused an item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    /// Tagged with an epoch that is not the running one. The item was
    /// quarantined for diagnostics if there was room, and counted either way.
    StaleEpoch,
    /// No cycle is running, so there is nobody to consume the item.
    Idle,
    /// The item carries no work (an empty batch).
    Empty,
}

// ── safepoint discipline ────────────────────────────────────────────────────

thread_local! {
    /// Depth of queue locks held by this thread. Only ever 0 or 1 in this
    /// module, but a counter keeps the guard composable if a future caller
    /// nests.
    static QUEUE_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// True while this thread holds any mark-queue lock.
pub fn queue_lock_held() -> bool {
    QUEUE_LOCK_DEPTH.with(|depth| depth.get() > 0)
}

/// Panics if the caller holds a mark-queue lock.
///
/// The safepoint wait path calls this: blocking at a safepoint while holding a
/// queue lock deadlocks the collector, because the thread that would release
/// the world is itself waiting behind the lock. Enforcing it here turns a rule
/// that would otherwise live in a comment into a loud, immediate failure.
pub fn assert_no_queue_lock_held(context: &str) {
    assert!(
        !queue_lock_held(),
        "mark-queue lock held at a safepoint wait ({context}): a blocked lock \
         holder deadlocks the collector"
    );
}

/// A mutex guard that records lock ownership for [`assert_no_queue_lock_held`].
struct QueueGuard<'a, T> {
    inner: MutexGuard<'a, T>,
}

impl<'a, T> QueueGuard<'a, T> {
    /// A poisoned mark queue is recovered rather than propagated: a mark worker
    /// that panics must not take the whole collector down with it, and the
    /// queue's invariants are counter-based, not guard-based.
    fn new(mutex: &'a Mutex<T>) -> Self {
        let inner = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        QUEUE_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self { inner }
    }
}

impl<T> std::ops::Deref for QueueGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> std::ops::DerefMut for QueueGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> Drop for QueueGuard<'_, T> {
    fn drop(&mut self) {
        QUEUE_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

// ── counters ────────────────────────────────────────────────────────────────

/// Saturating decrement. See the module's counter ordering contract: going
/// negative is a bug in this module, so it is caught in debug builds and
/// recorded in release builds instead of wrapping to `u64::MAX`.
fn sub_counter(counter: &AtomicU64, amount: u64, underflows: &AtomicU64) {
    if amount == 0 {
        return;
    }
    let previous = counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            Some(current.saturating_sub(amount))
        })
        .expect("saturating update never fails");
    if previous < amount {
        underflows.fetch_add(1, Ordering::SeqCst);
        debug_assert!(false, "mark queue counter underflow: {previous} - {amount}");
    }
}

#[derive(Default)]
struct Counters {
    /// **Authoritative.** Items accepted and not yet completed, wherever they
    /// are: injector, shared segment, private segment, or a consumer's hand.
    /// Segment-to-segment movement leaves it alone, so a single load of it is a
    /// sound termination predicate. See the module's termination accounting.
    outstanding: AtomicU64,
    /// Advisory: items reachable by any consumer (injector plus every shared
    /// segment). Diagnostics and pacing only.
    published: AtomicU64,
    /// Advisory: items in owner-private segments — work a peer cannot see. A
    /// non-zero reading is the termination protocol's cue to flush locals.
    hidden: AtomicU64,
    /// Advisory: consumers currently searching for or holding work.
    active_consumers: AtomicU64,
    /// Slots currently held by a background mark worker.
    registered_workers: AtomicU64,
    /// Slotless or slotted consumers registered as Mutator Assist.
    registered_assists: AtomicU64,
    injected: AtomicU64,
    delivered: AtomicU64,
    republished: AtomicU64,
    steals: AtomicU64,
    steal_attempts: AtomicU64,
    stale_rejected: AtomicU64,
    quarantined: AtomicU64,
    counter_underflows: AtomicU64,
    abandoned: AtomicU64,
}

/// A view of the queue for pacing, diagnostics, and termination detection.
///
/// The fields are read one at a time, so a snapshot taken while producers run
/// is a *sample* and not an atomic photograph — two fields in one snapshot may
/// describe different instants. Only [`Self::outstanding`] is load-bearing, and
/// [`Self::is_drained`] reads nothing else, so the termination decision never
/// depends on two fields agreeing. Everything else is advisory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MarkQueueSnapshot {
    pub epoch: MarkEpoch,
    /// Items accepted and not yet completed, anywhere — including in a
    /// consumer's hand. The one field termination detection may trust.
    pub outstanding: u64,
    /// Advisory: items in the injector and shared segments at some instant.
    pub published: u64,
    /// Advisory: items in private segments at some instant. Non-zero means a
    /// flush would make work visible to more consumers.
    pub hidden: u64,
    pub active_consumers: u64,
    pub registered_workers: u64,
    pub registered_assists: u64,
    pub injected: u64,
    pub delivered: u64,
    /// Items handed back by a retiring or failing consumer. Each one is a
    /// licensed duplicate delivery, so this is the measured cost of the
    /// at-least-once contract.
    pub republished: u64,
    pub steals: u64,
    pub steal_attempts: u64,
    pub stale_rejected: u64,
    pub quarantined: u64,
    pub counter_underflows: u64,
    /// Items a consumer took and then dropped without completing or
    /// relinquishing. Always a bug in the consumer; counted so it shows up as a
    /// number instead of as a hung cycle.
    pub abandoned: u64,
}

impl MarkQueueSnapshot {
    /// True when the queue held no work at the instant [`Self::outstanding`]
    /// was read — nothing queued and nothing in any consumer's hand.
    ///
    /// This is one atomic load, deliberately: a predicate built from several
    /// counters could be torn across a producer's update and report an empty
    /// queue that was never empty. Producers outside the consumer pool can
    /// still inject after the read, so a caller ending a cycle on this must
    /// quiesce them first.
    pub fn is_drained(&self) -> bool {
        self.outstanding == 0
    }
}

// ── local slots ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConsumerKind {
    Worker,
    Assist,
}

/// One consumer's storage.
///
/// `private` and `shared` are separate mutexes so a thief scanning `shared`
/// never contends with the owner's hot LIFO path.
///
/// # Lock order
///
/// ```text
/// injector < shared[0] < shared[1] < ... < private[0] < private[1] < ...
/// ```
///
/// Every two-lock path obeys it: publishing and flushing take `shared` then
/// `private` even though the work moves the other way, a steal takes the
/// victim's `shared` then the thief's `private`, and draining the injector takes
/// `injector` then `private`. The order matters — a flush that took `private`
/// first would deadlock against the slot's owner refilling from its own
/// `shared`.
///
/// An epoch transition ([`MarkWorkQueue::begin_epoch`],
/// [`MarkWorkQueue::end_epoch`]) is the only path that holds more than two: it
/// takes them all, ascending, which is why the order is total across slots and
/// not merely a three-tier rule.
struct LocalSlot {
    in_use: AtomicBool,
    private: Mutex<Vec<MarkWork>>,
    shared: Mutex<VecDeque<MarkWork>>,
}

impl Default for LocalSlot {
    fn default() -> Self {
        Self {
            in_use: AtomicBool::new(false),
            private: Mutex::new(Vec::new()),
            shared: Mutex::new(VecDeque::new()),
        }
    }
}

// ── the queue ───────────────────────────────────────────────────────────────

/// The mark work queue for one collector.
///
/// Construct with [`MarkWorkQueue::new`], wrap in an [`Arc`], and hand each
/// mark worker a [`MarkWorker`] from [`MarkWorkQueue::register_worker`].
pub struct MarkWorkQueue {
    /// The running cycle, or [`MarkEpoch::IDLE`] between cycles.
    epoch: AtomicU64,
    /// The highest epoch ever issued. Separate from `epoch` because ending a
    /// cycle parks `epoch` at the idle sentinel, and deriving the next cycle
    /// from a parked value would hand out an epoch number twice — see the
    /// module's epoch rules.
    last_issued: AtomicU64,
    injector: Mutex<VecDeque<MarkWork>>,
    slots: Vec<LocalSlot>,
    quarantine: Mutex<Vec<MarkWork>>,
    counters: Counters,
}

impl MarkWorkQueue {
    /// `slots` bounds the number of consumers with a private segment.
    ///
    /// Size it for the background worker pool **plus** the number of Mutator
    /// Assist consumers expected to run at once: assist claims a free slot when
    /// there is one, so a queue sized to the worker count alone leaves a worker
    /// without a slot whenever an assist registers first. Assists beyond the
    /// slot count still work — see [`MarkWorkQueue::register_assist`] — they
    /// simply publish straight to the injector instead of keeping local work.
    pub fn new(slots: usize) -> Arc<Self> {
        Arc::new(Self {
            epoch: AtomicU64::new(MarkEpoch::IDLE.0),
            last_issued: AtomicU64::new(MarkEpoch::IDLE.0),
            injector: Mutex::new(VecDeque::new()),
            slots: (0..slots).map(|_| LocalSlot::default()).collect(),
            quarantine: Mutex::new(Vec::new()),
            counters: Counters::default(),
        })
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn current_epoch(&self) -> MarkEpoch {
        MarkEpoch(self.epoch.load(Ordering::SeqCst))
    }

    /// Start a new mark cycle and return its epoch.
    ///
    /// The epoch is drawn from a monotonic counter, so it is always one this
    /// queue has never issued before — including across an
    /// [`Self::end_epoch`]. Any work left from a previous cycle is discarded:
    /// a restart (willow-6fv.5.6.5) begins from a fresh root set, so carrying
    /// stale items forward would only re-trace objects under the wrong tag.
    ///
    /// Callers are expected to have quiesced consumers first — this is a
    /// safepoint operation — but correctness does not depend on it. The
    /// transition is atomic against producers, so a concurrent `inject` or
    /// `push` either lands wholly in the old cycle (and is discarded here) or
    /// is refused as stale. Neither can leak an old-epoch item into the new
    /// cycle.
    pub fn begin_epoch(&self) -> MarkEpoch {
        let previous = self
            .last_issued
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |issued| {
                issued.checked_add(1)
            })
            .expect("mark epoch exhausted");
        let next = MarkEpoch(
            previous
                .checked_add(1)
                .expect("successful epoch update must have a successor"),
        );
        self.transition_to(next);
        next
    }

    /// End the current cycle, discard everything still queued, and return how
    /// many items were dropped. A non-zero return from a *terminated* cycle
    /// means termination detection concluded early — worth asserting on.
    ///
    /// The epoch counter is deliberately not rewound: the next
    /// [`Self::begin_epoch`] issues a fresh number, never this one again.
    /// Handles outstanding across this call become stale and stop producing
    /// work rather than misbehaving.
    pub fn end_epoch(&self) -> usize {
        self.transition_to(MarkEpoch::IDLE)
    }

    /// Move the queue to `next`, discarding everything queued, and return how
    /// many items were dropped.
    ///
    /// Holds **every** lock — injector, then each shared segment, then each
    /// private segment, in the module's total order — for the whole
    /// transition. That is what makes an epoch change atomic with respect to
    /// producers: `inject` and `push` validate the epoch under the lock they
    /// insert beneath, so between their check and their insertion this function
    /// cannot run. Without it a producer could check epoch N, be preempted
    /// while this call discards the queue and moves to N+1, and then push
    /// N-tagged work into a cycle that has already cleared its mark bits.
    fn transition_to(&self, next: MarkEpoch) -> usize {
        let mut injector = QueueGuard::new(&self.injector);
        let mut shared: Vec<_> = self
            .slots
            .iter()
            .map(|slot| QueueGuard::new(&slot.shared))
            .collect();
        let mut private: Vec<_> = self
            .slots
            .iter()
            .map(|slot| QueueGuard::new(&slot.private))
            .collect();

        let mut discarded = injector.len();
        injector.clear();
        for queue in &mut shared {
            discarded += queue.len();
            queue.clear();
        }
        for queue in &mut private {
            discarded += queue.len();
            queue.clear();
        }

        // The counters describe contents that no longer exist, so they go to
        // zero rather than being decremented item by item. `outstanding` also
        // covers items in consumers' hands: those consumers are now stale, and
        // a stale consumer completes without decrementing (it compares the tag
        // it is holding against the current epoch), so this cannot underflow.
        // `active_consumers` belongs to the consumers rather than the contents
        // and is left alone.
        self.counters.outstanding.store(0, Ordering::SeqCst);
        self.counters.published.store(0, Ordering::SeqCst);
        self.counters.hidden.store(0, Ordering::SeqCst);
        self.epoch.store(next.0, Ordering::SeqCst);
        discarded
    }

    /// Publish work from outside the consumer pool: root jobs, SATB buffer
    /// flushes, the remark pause.
    ///
    /// Returns `Err` for work that will never be marked, so producers can count
    /// their own losses instead of assuming success.
    ///
    /// The epoch check and the push happen under one hold of the injector lock.
    /// Reading the epoch first and pushing afterwards would leave a window for
    /// [`Self::transition_to`] to discard the queue in between, and the item
    /// would then be delivered under a cycle that never asked for it.
    pub fn inject(&self, work: MarkWork) -> Result<(), RejectReason> {
        if work.item.is_empty() {
            return Err(RejectReason::Empty);
        }
        let refused = {
            let mut injector = QueueGuard::new(&self.injector);
            let current = self.current_epoch();
            if current.is_idle() {
                (work, RejectReason::Idle)
            } else if work.epoch != current {
                (work, RejectReason::StaleEpoch)
            } else {
                // Counter before visibility: an observer that sees
                // `outstanding == 0` must never be looking at a queue that
                // already contains this item. Both happen under this lock, so
                // no epoch transition can separate them.
                self.counters.outstanding.fetch_add(1, Ordering::SeqCst);
                self.counters.published.fetch_add(1, Ordering::SeqCst);
                self.counters.injected.fetch_add(1, Ordering::SeqCst);
                injector.push_back(work);
                return Ok(());
            }
        };
        // Quarantining takes another lock, so it happens after the injector
        // lock is released.
        let (work, reason) = refused;
        self.reject(work, reason);
        Err(reason)
    }

    /// Inject many items, returning how many were accepted. Items are tagged
    /// individually, so a producer that straddles an epoch boundary loses only
    /// the stale ones.
    pub fn inject_batch(&self, items: impl IntoIterator<Item = MarkWork>) -> usize {
        let mut accepted = 0;
        for work in items {
            if self.inject(work).is_ok() {
                accepted += 1;
            }
        }
        accepted
    }

    fn reject(&self, work: MarkWork, reason: RejectReason) {
        if reason == RejectReason::StaleEpoch || reason == RejectReason::Idle {
            self.counters.stale_rejected.fetch_add(1, Ordering::SeqCst);
            let mut quarantine = QueueGuard::new(&self.quarantine);
            if quarantine.len() < MAX_QUARANTINED_ITEMS {
                quarantine.push(work);
                self.counters.quarantined.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Drain the quarantine for inspection. Diagnostics only — quarantined work
    /// is never re-delivered, because it belongs to a cycle that is over.
    pub fn take_quarantine(&self) -> Vec<MarkWork> {
        let mut quarantine = QueueGuard::new(&self.quarantine);
        let taken = std::mem::take(&mut *quarantine);
        sub_counter(
            &self.counters.quarantined,
            taken.len() as u64,
            &self.counters.counter_underflows,
        );
        taken
    }

    /// Register a background mark worker. Returns `None` when every slot is
    /// taken; the caller should not have spawned more workers than slots.
    pub fn register_worker(self: &Arc<Self>) -> Option<MarkWorker> {
        let slot = self.claim_slot()?;
        self.counters
            .registered_workers
            .fetch_add(1, Ordering::SeqCst);
        Some(MarkWorker::new(
            Arc::clone(self),
            Some(slot),
            ConsumerKind::Worker,
        ))
    }

    /// Register a Mutator Assist consumer.
    ///
    /// Assist gets *the same consumer interface* as a mark worker — the same
    /// type, the same `next_work`/`push`/`relinquish` methods — so assist code
    /// cannot drift from worker code. The only difference is that assist is
    /// unbounded in number: when no slot is free the handle runs slotless,
    /// consuming normally but publishing straight to the injector, which costs
    /// a little throughput and keeps correctness identical.
    pub fn register_assist(self: &Arc<Self>) -> MarkWorker {
        let slot = self.claim_slot();
        self.counters
            .registered_assists
            .fetch_add(1, Ordering::SeqCst);
        MarkWorker::new(Arc::clone(self), slot, ConsumerKind::Assist)
    }

    fn claim_slot(&self) -> Option<usize> {
        self.slots.iter().position(|slot| {
            slot.in_use
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
    }

    /// Force every private segment into the injector so all work is visible to
    /// every consumer. Returns the number of items moved.
    ///
    /// This is how termination stops chasing hidden work: after this call
    /// `hidden` is zero for the slots that existed, and anything a consumer
    /// pushes afterwards is published or hidden under the normal rules, so the
    /// termination protocol only has to re-check the counters.
    pub fn flush_all_locals(&self) -> usize {
        let mut moved = 0;
        for index in 0..self.slots.len() {
            moved += self.flush_slot(index);
        }
        moved
    }

    /// Drain one slot's private and shared segments into the injector.
    ///
    /// Shared items were already counted as published, so only the private
    /// share moves between counters; the increment happens before the
    /// decrement, so the items are briefly counted twice and never zero times.
    fn flush_slot(&self, index: usize) -> usize {
        let slot = &self.slots[index];
        let (hidden_count, drained) = {
            // Lock order: shared before private. See `LocalSlot`.
            let mut shared = QueueGuard::new(&slot.shared);
            let mut private = QueueGuard::new(&slot.private);
            let hidden_count = private.len() as u64;
            let mut drained = Vec::with_capacity(private.len() + shared.len());
            drained.extend(private.drain(..));
            drained.extend(shared.drain(..));
            (hidden_count, drained)
        };
        if drained.is_empty() {
            return 0;
        }
        let total = drained.len();
        self.counters
            .published
            .fetch_add(hidden_count, Ordering::SeqCst);
        {
            let mut injector = QueueGuard::new(&self.injector);
            injector.extend(drained);
        }
        sub_counter(
            &self.counters.hidden,
            hidden_count,
            &self.counters.counter_underflows,
        );
        total
    }

    pub fn snapshot(&self) -> MarkQueueSnapshot {
        let counters = &self.counters;
        MarkQueueSnapshot {
            epoch: self.current_epoch(),
            outstanding: counters.outstanding.load(Ordering::SeqCst),
            published: counters.published.load(Ordering::SeqCst),
            hidden: counters.hidden.load(Ordering::SeqCst),
            active_consumers: counters.active_consumers.load(Ordering::SeqCst),
            registered_workers: counters.registered_workers.load(Ordering::SeqCst),
            registered_assists: counters.registered_assists.load(Ordering::SeqCst),
            injected: counters.injected.load(Ordering::SeqCst),
            delivered: counters.delivered.load(Ordering::SeqCst),
            republished: counters.republished.load(Ordering::SeqCst),
            steals: counters.steals.load(Ordering::SeqCst),
            steal_attempts: counters.steal_attempts.load(Ordering::SeqCst),
            stale_rejected: counters.stale_rejected.load(Ordering::SeqCst),
            quarantined: counters.quarantined.load(Ordering::SeqCst),
            counter_underflows: counters.counter_underflows.load(Ordering::SeqCst),
            abandoned: counters.abandoned.load(Ordering::SeqCst),
        }
    }
}

// ── consumer handle ─────────────────────────────────────────────────────────

/// A consumer's view of the queue: one mark worker, or one Mutator Assist.
///
/// Not `Clone` and not `Sync` — a slot has exactly one owner, which is what
/// makes the private segment safe to treat as unstealable.
pub struct MarkWorker {
    queue: Arc<MarkWorkQueue>,
    slot: Option<usize>,
    kind: ConsumerKind,
    epoch: MarkEpoch,
    active: bool,
    /// The epoch tag of the item this consumer was last handed and has not
    /// completed yet. It keeps that item counted in `outstanding` for as long
    /// as the consumer holds it, which is what closes the gap between "removed
    /// from a segment" and "actually marked". Storing the tag rather than a
    /// flag lets a completion after an epoch change skip the decrement: the
    /// transition already zeroed the counter.
    in_hand: Option<MarkEpoch>,
    /// Rotates so an idle worker does not hammer the same victim.
    next_victim: usize,
    retired: bool,
}

impl MarkWorker {
    fn new(queue: Arc<MarkWorkQueue>, slot: Option<usize>, kind: ConsumerKind) -> Self {
        let epoch = queue.current_epoch();
        let next_victim = slot.map(|slot| slot + 1).unwrap_or(0);
        Self {
            queue,
            slot,
            kind,
            epoch,
            active: false,
            in_hand: None,
            next_victim,
            retired: false,
        }
    }

    pub fn epoch(&self) -> MarkEpoch {
        self.epoch
    }

    pub fn slot(&self) -> Option<usize> {
        self.slot
    }

    pub fn is_assist(&self) -> bool {
        self.kind == ConsumerKind::Assist
    }

    /// True once the collector has moved on: the handle stops producing work
    /// and its pushes are rejected.
    pub fn is_stale(&self) -> bool {
        self.queue.current_epoch() != self.epoch
    }

    /// Push work discovered while marking.
    ///
    /// Goes to the private LIFO segment, keeping tracing depth-first, and
    /// spills half to the shared segment once the private one passes
    /// [`LOCAL_PUBLISH_THRESHOLD`] so peers always have something to steal. A
    /// slotless assist publishes to the injector instead.
    ///
    /// Like [`MarkWorkQueue::inject`], the epoch is validated under the same
    /// lock the item is inserted beneath, so an epoch transition cannot land
    /// between the check and the insertion.
    pub fn push(&mut self, item: MarkWorkItem) -> Result<(), RejectReason> {
        if item.is_empty() {
            return Err(RejectReason::Empty);
        }
        // Fast path only: the authoritative check is under the lock below.
        if self.is_stale() {
            let work = MarkWork::new(self.epoch, item);
            self.queue.reject(work, RejectReason::StaleEpoch);
            return Err(RejectReason::StaleEpoch);
        }
        let work = MarkWork::new(self.epoch, item);
        let Some(slot) = self.slot else {
            return self.queue.inject(work);
        };
        let slot = &self.queue.slots[slot];
        let outcome = {
            let mut private = QueueGuard::new(&slot.private);
            if self.queue.current_epoch() != self.epoch {
                Err(work)
            } else {
                self.queue
                    .counters
                    .outstanding
                    .fetch_add(1, Ordering::SeqCst);
                self.queue.counters.hidden.fetch_add(1, Ordering::SeqCst);
                private.push(work);
                Ok(private.len() > LOCAL_PUBLISH_THRESHOLD)
            }
        };
        match outcome {
            Ok(overflow) => {
                if overflow {
                    self.publish_local(usize::MAX);
                }
                Ok(())
            }
            // Quarantining takes another lock; do it once the private lock is
            // released.
            Err(work) => {
                self.queue.reject(work, RejectReason::StaleEpoch);
                Err(RejectReason::StaleEpoch)
            }
        }
    }

    /// Move up to `limit` items (default: half the private segment) from the
    /// private LIFO into the shared segment, oldest first.
    ///
    /// Oldest first is deliberate: the owner's LIFO pops the newest items, so
    /// the oldest are the ones it is least likely to want next and the ones a
    /// thief can take with the least cache damage. A non-empty private segment
    /// always donates at least one item, so `limit` can never round a donation
    /// down to nothing and strand the work.
    pub fn publish_local(&mut self, limit: usize) -> usize {
        let Some(slot) = self.slot else {
            return 0;
        };
        let slot = &self.queue.slots[slot];
        // Lock order: shared before private, even though the work moves the
        // other way. See `LocalSlot`.
        let mut shared = QueueGuard::new(&slot.shared);
        let mut private = QueueGuard::new(&slot.private);
        if private.is_empty() {
            return 0;
        }
        let half = private.len().div_ceil(2);
        let count = limit.min(half).max(1).min(private.len());
        let moved: Vec<MarkWork> = private.drain(..count).collect();
        let moved_count = moved.len() as u64;
        // Destination before source, as everywhere else.
        self.queue
            .counters
            .published
            .fetch_add(moved_count, Ordering::SeqCst);
        shared.extend(moved);
        drop(private);
        drop(shared);
        sub_counter(
            &self.queue.counters.hidden,
            moved_count,
            &self.queue.counters.counter_underflows,
        );
        moved_count as usize
    }

    /// Take the next item, or `None` when this consumer can find no work.
    ///
    /// Search order is private LIFO, own shared segment, injector, then a
    /// bounded steal from each peer in rotation.
    ///
    /// **Asking for the next item completes the previous one.** The item this
    /// consumer was last handed stays counted in `outstanding` until then, so
    /// termination detection never sees an empty queue while a worker is still
    /// tracing. A consumer that will not finish an item must hand it back with
    /// [`Self::relinquish`] instead of simply asking for another.
    ///
    /// `None` does **not** mean marking is finished: a peer may still publish.
    /// Only the termination protocol decides that.
    pub fn next_work(&mut self) -> Option<MarkWork> {
        self.complete_held();
        if self.is_stale() {
            self.deactivate();
            return None;
        }
        self.activate();

        while let Some(work) = self.find_work() {
            // Backstop. An epoch transition holds every lock and clears every
            // segment, so a stale item should not be reachable at all; if one
            // ever is, quarantine it rather than mark an address that belongs
            // to a finished cycle.
            if work.epoch != self.queue.current_epoch() {
                self.queue.reject(work, RejectReason::StaleEpoch);
                continue;
            }
            return Some(self.deliver(work));
        }
        self.deactivate();
        None
    }

    /// One pass of the search order. Separate from [`Self::next_work`] so a
    /// rejected item can be retried without re-running the active/stale
    /// bookkeeping.
    fn find_work(&mut self) -> Option<MarkWork> {
        if let Some(work) = self.pop_private() {
            return Some(work);
        }
        if self.refill_from_own_shared() > 0
            && let Some(work) = self.pop_private()
        {
            return Some(work);
        }
        if let Some(work) = self.take_from_injector() {
            return Some(work);
        }
        if self.steal_from_peers() > 0
            && let Some(work) = self.pop_private()
        {
            return Some(work);
        }
        None
    }

    /// Hand back an item this consumer will not process — it is retiring, or it
    /// hit an error mid-item.
    ///
    /// The item is republished to the injector and counted in
    /// [`MarkQueueSnapshot::republished`]: whoever picks it up may be repeating
    /// work already done, which the at-least-once contract permits and marking
    /// tolerates.
    ///
    /// This moves the item from a hand back into the queue, so `outstanding` is
    /// unchanged — it counted the item in both places. An item this consumer
    /// was never handed is a new insertion and is counted as one.
    pub fn relinquish(&mut self, work: MarkWork) {
        let was_held = self.in_hand.take().is_some();
        let refused = {
            let mut injector = QueueGuard::new(&self.queue.injector);
            if work.epoch != self.queue.current_epoch() {
                Some(work)
            } else {
                if !was_held {
                    self.queue
                        .counters
                        .outstanding
                        .fetch_add(1, Ordering::SeqCst);
                }
                self.queue
                    .counters
                    .republished
                    .fetch_add(1, Ordering::SeqCst);
                self.queue.counters.published.fetch_add(1, Ordering::SeqCst);
                injector.push_back(work);
                return;
            }
        };
        if let Some(work) = refused {
            // A held item that has gone stale was already dropped from
            // `outstanding` by the epoch transition, so there is nothing to
            // undo here.
            self.queue.reject(work, RejectReason::StaleEpoch);
        }
    }

    /// Release the slot and publish everything left in it.
    ///
    /// Called automatically on drop. A worker that exits with local work must
    /// not take that work with it — the items are published to the injector so
    /// a surviving consumer finds them.
    ///
    /// An item still *in hand* cannot be recovered: this handle gave ownership
    /// of it to the caller, who should have called [`Self::relinquish`]. It is
    /// completed here so the count cannot strand a cycle forever, and recorded
    /// in [`MarkQueueSnapshot::abandoned`] so the mistake is visible as a number
    /// rather than as an object that quietly never got marked.
    pub fn retire(&mut self) -> usize {
        if self.retired {
            return 0;
        }
        self.retired = true;
        if self.in_hand.is_some() {
            self.queue.counters.abandoned.fetch_add(1, Ordering::SeqCst);
            self.complete_held();
        }
        self.deactivate();
        let moved = match self.slot {
            Some(index) => {
                let moved = self.queue.flush_slot(index);
                self.queue.slots[index]
                    .in_use
                    .store(false, Ordering::SeqCst);
                moved
            }
            None => 0,
        };
        let counter = match self.kind {
            ConsumerKind::Worker => &self.queue.counters.registered_workers,
            ConsumerKind::Assist => &self.queue.counters.registered_assists,
        };
        sub_counter(counter, 1, &self.queue.counters.counter_underflows);
        moved
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn deliver(&mut self, work: MarkWork) -> MarkWork {
        self.queue.counters.delivered.fetch_add(1, Ordering::SeqCst);
        // The item leaves the queue's storage but stays counted: `outstanding`
        // covers hands as well as segments.
        self.in_hand = Some(work.epoch);
        work
    }

    /// Drop the claim on the item this consumer was handed, decrementing
    /// `outstanding` exactly once.
    ///
    /// A tag from a finished cycle is skipped: [`MarkWorkQueue::transition_to`]
    /// zeroed the counter with that item included, so decrementing again would
    /// underflow.
    fn complete_held(&mut self) {
        let Some(held) = self.in_hand.take() else {
            return;
        };
        if held != self.queue.current_epoch() {
            return;
        }
        sub_counter(
            &self.queue.counters.outstanding,
            1,
            &self.queue.counters.counter_underflows,
        );
    }

    fn activate(&mut self) {
        if !self.active {
            self.active = true;
            self.queue
                .counters
                .active_consumers
                .fetch_add(1, Ordering::SeqCst);
        }
    }

    fn deactivate(&mut self) {
        if self.active {
            self.active = false;
            sub_counter(
                &self.queue.counters.active_consumers,
                1,
                &self.queue.counters.counter_underflows,
            );
        }
    }

    fn pop_private(&self) -> Option<MarkWork> {
        let slot = &self.queue.slots[self.slot?];
        let work = QueueGuard::new(&slot.private).pop()?;
        sub_counter(
            &self.queue.counters.hidden,
            1,
            &self.queue.counters.counter_underflows,
        );
        Some(work)
    }

    fn refill_from_own_shared(&self) -> usize {
        let Some(index) = self.slot else {
            return 0;
        };
        drain_into_private(
            &self.queue,
            index,
            &self.queue.slots[index].shared,
            MAX_STEAL_BATCH,
        )
    }

    fn take_from_injector(&self) -> Option<MarkWork> {
        if let Some(index) = self.slot {
            if drain_into_private(&self.queue, index, &self.queue.injector, MAX_STEAL_BATCH) > 0 {
                return self.pop_private();
            }
            return None;
        }
        // A slotless assist has nowhere to stage a batch, so it takes one item.
        let work = QueueGuard::new(&self.queue.injector).pop_front()?;
        sub_counter(
            &self.queue.counters.published,
            1,
            &self.queue.counters.counter_underflows,
        );
        Some(work)
    }

    /// Try each peer once, in rotation, taking at most [`MAX_STEAL_BATCH`]
    /// items or half the victim's shared segment.
    ///
    /// Half rather than all: leaving the victim with work avoids ping-ponging
    /// the same items between two idle consumers.
    fn steal_from_peers(&mut self) -> usize {
        let slot_count = self.queue.slot_count();
        let Some(own) = self.slot else {
            return 0;
        };
        if slot_count < 2 {
            return 0;
        }
        for offset in 0..slot_count {
            let victim = (self.next_victim + offset) % slot_count;
            if victim == own {
                continue;
            }
            self.queue
                .counters
                .steal_attempts
                .fetch_add(1, Ordering::SeqCst);
            let available = QueueGuard::new(&self.queue.slots[victim].shared).len();
            if available == 0 {
                continue;
            }
            let limit = MAX_STEAL_BATCH.min(available.div_ceil(2)).max(1);
            let taken =
                drain_into_private(&self.queue, own, &self.queue.slots[victim].shared, limit);
            if taken > 0 {
                self.queue.counters.steals.fetch_add(1, Ordering::SeqCst);
                self.next_victim = (victim + 1) % slot_count;
                return taken;
            }
        }
        0
    }
}

/// Move a bounded batch out of a shared `VecDeque` into `slot_index`'s private
/// segment, keeping the counters ordered destination-first.
///
/// Free function rather than a method so the source mutex can be borrowed from
/// the queue while the private segment is written, without aliasing a `&mut`
/// handle.
fn drain_into_private(
    queue: &MarkWorkQueue,
    slot_index: usize,
    source: &Mutex<VecDeque<MarkWork>>,
    limit: usize,
) -> usize {
    let taken: Vec<MarkWork> = {
        let mut source = QueueGuard::new(source);
        let count = limit.min(source.len());
        source.drain(..count).collect()
    };
    if taken.is_empty() {
        return 0;
    }
    let count = taken.len() as u64;
    queue.counters.hidden.fetch_add(count, Ordering::SeqCst);
    {
        let mut private = QueueGuard::new(&queue.slots[slot_index].private);
        // Reverse so the oldest taken item ends up deepest: the owner pops LIFO,
        // and taking the oldest first keeps a stolen run in the order the victim
        // would have run it.
        private.extend(taken.into_iter().rev());
    }
    sub_counter(
        &queue.counters.published,
        count,
        &queue.counters.counter_underflows,
    );
    count as usize
}

impl Drop for MarkWorker {
    fn drop(&mut self) {
        self.retire();
    }
}

#[cfg(test)]
#[path = "gc_mark_queue_tests.rs"]
mod gc_mark_queue_tests;

#[cfg(test)]
#[path = "gc_mark_queue_example.rs"]
mod gc_mark_queue_example;
