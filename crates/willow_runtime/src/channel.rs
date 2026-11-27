//! Channel state, GC integration, ownership cleanup, and wake dispatch.

mod abi;
mod deferred;
#[cfg(test)]
mod deferred_tests;
pub use abi::*;

use crate::id_hash::IdMap;
use crate::native_memory::Mutex;
use crate::task::{ChannelOwnershipToken, ChannelRole};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;

use crate::trace::{GcTrace, GcVisitor};

#[repr(C)]
#[derive(Clone, Copy)]
pub union WillowChannelValue {
    pub i64_value: i64,
    pub bool_value: u8,
    pub f64_value: f64,
    pub ptr_value: *mut c_void,
}

impl Default for WillowChannelValue {
    fn default() -> Self {
        Self { i64_value: 0 }
    }
}

// Channels need bounded unlink/pop work during select cleanup. Task-completion
// waits retain the separate shared queue and its amortized tombstone policy.
mod waiters;
use waiters::WaiterQueue;

#[derive(Default)]
struct WillowChannelState {
    values: VecDeque<WillowChannelValue>,
    /// Finite major snapshot: pops adjust the offset into the current deque;
    /// appends do not extend remaining work. No lifetime sequence can wrap.
    gc_scan_position: usize,
    gc_scan_remaining: usize,
    closed: bool,
    /// Cooperative consumers parked on an empty `recv`, woken FIFO by `send` /
    /// `close`.
    waiters: WaiterQueue,
    /// Bounded capacity (`Channel<T>::with_capacity(n)`); `None` = unbounded.
    ///
    capacity: Option<usize>,
    /// Cooperative producers parked on a FULL bounded channel. A `recv` that
    /// frees one slot wakes one live producer; `close` wakes all.
    send_waiters: WaiterQueue,
    // Values remain in the traced queue; claims reserve availability, not slots
    // in movable task frames. Reservations cannot outnumber queued values.
    recv_claims: IdMap<u64, u64>,
    send_handoffs: IdMap<u64, u64>,
    next_generation: u64,
    native_tables: [usize; 2],
    deferred_wakes: [bool; 2],
    /// Mirror of `Inbox::open`, written only with both locks held.
    inbox_open: bool,
}

/// Sends that arrive while no receiver waits on an open unbounded channel.
/// They append here under this lock alone, so producers stop contending with
/// the receiver for the state lock; a receiver that finds no unreserved value
/// in `values` moves the whole inbox there with one acquisition. The logical
/// queue is `values` followed by `values` of the inbox.
///
/// Invariants: the inbox is open only for an unbounded, unclosed channel with
/// no receive waiters, and it is empty while closed. The state lock is always
/// taken before this one, and `open` changes only with both held, so every
/// state-lock holder that sees `inbox_open == false` sees an empty inbox and
/// every waiter, claim and close decision keeps its single-queue meaning.
#[derive(Default)]
struct Inbox {
    values: VecDeque<WillowChannelValue>,
    open: bool,
}

impl crate::native_memory::Footprint for Inbox {
    fn native_bytes(&mut self) -> usize {
        self.values.capacity() * size_of::<WillowChannelValue>()
    }
}

impl crate::native_memory::Footprint for WillowChannelState {
    fn native_bytes(&mut self) -> usize {
        crate::native_memory::sum_bytes([
            self.values.capacity() * size_of::<WillowChannelValue>(),
            self.waiters.native_bytes(),
            self.send_waiters.native_bytes(),
            crate::native_memory::retain_capacity(
                &mut self.native_tables[0],
                crate::native_memory::hash_bytes::<u64, u64>(self.recv_claims.capacity()),
            ),
            crate::native_memory::retain_capacity(
                &mut self.native_tables[1],
                crate::native_memory::hash_bytes::<u64, u64>(self.send_handoffs.capacity()),
            ),
        ])
    }
}

pub struct WillowAbiChannel {
    // Native synchronization and cancellation identity stay at a stable address
    // when the GC-owned wrapper is relocated. The rooted frame retains ownership.
    inner: Box<ChannelCore>,
}

pub struct ChannelCore {
    state: Mutex<WillowChannelState>,
    inbox: Mutex<Inbox>,
    /// Lock-free hint of `Inbox::open` so a send to a closed inbox (bounded
    /// channel, parked receiver) skips its lock; the lock decides.
    inbox_hint: std::sync::atomic::AtomicBool,
    /// True when the element type is a GC reference (String / class / array /
    /// ...): queued values are then GC roots scanned by the collector
    /// (channel payload GC tracing).
    is_ref: bool,
}

impl std::ops::Deref for WillowAbiChannel {
    type Target = ChannelCore;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl WillowAbiChannel {
    fn new(is_ref: bool) -> Self {
        Self {
            inner: Box::new(ChannelCore {
                state: Mutex::with_base(WillowChannelState::default(), size_of::<ChannelCore>()),
                inbox: Mutex::new(Inbox::default()),
                inbox_hint: std::sync::atomic::AtomicBool::new(false),
                is_ref,
            }),
        }
    }
}

/// GC type id for channel objects: channels are GC-MANAGED —
/// unreachable channels are reclaimed by the collector like any object, and
/// their queued reference values are traced by [`trace_channel`]. The old
/// program-lifetime leak + global registry (and its O(all-channels)
/// cancellation scan) are gone; cancellation uses task-side reverse
/// references instead.
use willow_abi::runtime_type_ids::CHANNEL_TYPE_ID;

/// Trace a channel payload: every queued value of a GC-element channel is a
/// child. Runs at stop-the-world, and no safepoint exists inside the send/
/// recv lock regions, so the state lock is never held by a stopped mutator.
///
/// # Safety
/// `payload` must be a [`WillowAbiChannel`] allocated by `willow_channel_new`.
unsafe fn trace_channel(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let channel = unsafe { &*(payload as *const WillowAbiChannel) };
    if !channel.is_ref {
        return;
    }
    // Queue operations preserve VecDeque's initialized-element invariant even
    // when another channel operation unwinds. Recover only for GC: bookkeeping
    // may be incomplete, but every remaining queued reference must stay alive.
    let mut state = channel
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for value in &mut state.values {
        slots.push(std::ptr::addr_of_mut!(value.ptr_value).cast::<*mut u8>());
    }
    // Inbox values are traced where they lie. Moving them into `values` here
    // could grow its buffer, and the guard's native-memory recharge would
    // take the heap lock the stopped-world collector already holds. With
    // every mutator stopped no push can move the inbox buffer before the
    // collector rewrites these slots.
    if state.inbox_open {
        let mut inbox = channel
            .inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for value in &mut inbox.values {
            slots.push(std::ptr::addr_of_mut!(value.ptr_value).cast::<*mut u8>());
        }
    }
}

unsafe fn snapshot_channel(payload: *mut u8, children: &mut Vec<*mut u8>) {
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let channel = unsafe { &*(payload as *const WillowAbiChannel) };
    if channel.is_ref {
        // As in STW tracing, poison must never suppress queued roots. Copy
        // directly into the caller's buffer; traversal/marking happens unlocked.
        let state = channel
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        children.extend(
            state
                .values
                .iter()
                // SAFETY: The channel element ABI supplies the initialized union representation
                // read by this operation.
                .map(|value| unsafe { value.ptr_value as *mut u8 }),
        );
        // Copied, never absorbed: GC paths must not change native accounting.
        if state.inbox_open {
            let inbox = channel
                .inbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            children.extend(
                inbox
                    .values
                    .iter()
                    // SAFETY: The channel element ABI supplies the initialized union
                    // representation read by this operation.
                    .map(|value| unsafe { value.ptr_value as *mut u8 }),
            );
        }
    }
}

unsafe fn snapshot_channel_slice(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    use crate::gc::TraceSliceProgress;
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let channel = unsafe { &*payload.cast::<WillowAbiChannel>() };
    if !channel.is_ref {
        return TraceSliceProgress::Done;
    }
    let mut state = match channel.state.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
    };
    // The scan covers the logical queue `values ++ inbox`. Absorbing the inbox
    // and closing it keep that order, receives pop at the front (adjusting the
    // window), and every send appends at the end, past the window, under the
    // send barrier. Inbox values are read in place, never absorbed: a GC path
    // must not change native accounting (its recharge can take the heap lock).
    let values_len = state.values.len();
    let needs_inbox = state.inbox_open
        && (cursor == 0
            || state.gc_scan_position + state.gc_scan_remaining.min(limit) > values_len);
    let inbox = if needs_inbox {
        match channel.inbox.try_lock() {
            Ok(inbox) => Some(inbox),
            Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
            Err(std::sync::TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
        }
    } else {
        None
    };
    if cursor == 0 {
        // Sends queued before marking began carry no barrier shading: the
        // snapshot must hold them, so the whole logical queue is scanned.
        state.gc_scan_position = 0;
        state.gc_scan_remaining = values_len + inbox.as_ref().map_or(0, |inbox| inbox.values.len());
    }
    let count = state.gc_scan_remaining.min(limit);
    let end = state.gc_scan_position + count;
    for index in state.gc_scan_position..end {
        // Indexed VecDeque access is O(1), including a wrapped backing buffer.
        let value = if index < values_len {
            &state.values[index]
        } else {
            &inbox
                .as_ref()
                .expect("a scan window past the queue lies in the open inbox")
                .values[index - values_len]
        };
        // SAFETY: The channel element ABI supplies the initialized union representation read by
        // this operation.
        children.push(unsafe { value.ptr_value }.cast());
    }
    drop(inbox);
    state.gc_scan_position = end;
    state.gc_scan_remaining -= count;
    if state.gc_scan_remaining == 0 {
        TraceSliceProgress::Done
    } else {
        TraceSliceProgress::Continue(cursor + count)
    }
}

/// Drop a channel payload before the GC releases its allocation. The channel
/// state owns Rust-allocated `VecDeque` buffers, so deallocating only the GC
/// block would leak those buffers.
///
/// # Safety
/// `payload` must point to an initialized [`WillowAbiChannel`].
unsafe fn drop_channel(payload: *mut u8) {
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    unsafe {
        std::ptr::drop_in_place(payload as *mut WillowAbiChannel);
    }
}

#[cfg(test)]
const CHANNEL_DROP_COUNT: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelDropCount);

const CHANNEL_REGISTRATION: crate::gc::NativeGcRegistration =
    crate::gc::NativeGcRegistration::Channel;
const CHANNEL_GC_TYPES: &[crate::gc::NativeGcType] = &[
    // SAFETY: This ID denotes the initialized channel wrapper and its stable native core.
    // Stopped tracing visits reference queues; concurrent snapshots synchronize with their
    // writers and bound the queue range.
    unsafe {
        crate::gc::NativeGcType::new(CHANNEL_TYPE_ID, Some(trace_channel), Some(drop_channel))
            .with_concurrent_trace(snapshot_channel)
            .with_concurrent_slice(snapshot_channel_slice)
    },
];

#[cfg(test)]
const CHANNEL_REGISTRATION_COUNT: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelRegistrationCount);

/// Register both GC hooks once per registry generation. The common channel
/// allocation path performs only two atomic loads; the mutex and registry
/// HashMap locks are taken only after a GC reset/unregister invalidates hooks.
fn ensure_channel_registered() {
    // SAFETY: This is the complete descriptor set for the channel wrapper registration group
    // and its constructors.
    if unsafe { CHANNEL_REGISTRATION.ensure(CHANNEL_GC_TYPES) } {
        crate::observation::record(crate::observation::Event::ChannelRegistrationCount, 1);
    }
}

/// Exercise the channel ABI with a wrapper that the next minor can evacuate.
#[cfg(test)]
pub(crate) fn young_channel_for_test(is_ref: bool) -> *mut u8 {
    ensure_channel_registered();
    let bytes = size_of::<WillowAbiChannel>() as i64;
    let type_id = i64::from(CHANNEL_TYPE_ID);
    let layout = crate::gc::gc_layout_id(crate::gc::GcObjectKind::Channel, bytes, type_id, 0);
    // SAFETY: The current mutator owns the runtime TLS block until retirement; this fixture
    // uses its word layout and inline mask.
    let raw = unsafe {
        crate::gc::willow_gc_alloc_slow(
            crate::gc::willow_gc_current_tlab(),
            layout,
            type_id,
            bytes,
            0,
        )
    };
    assert!(!raw.is_null());
    // SAFETY: The TLAB reserves word-aligned storage of the wrapper's exact size.
    // Initialize it before another allocation or collection can invoke the channel hooks.
    unsafe {
        raw.cast::<WillowAbiChannel>()
            .write(WillowAbiChannel::new(is_ref))
    };
    raw
}

/// Move every inbox value to the back of `values`: one inbox acquisition per
/// batch, O(1) when `values` is empty (the buffers swap). A no-op while the
/// inbox is closed, since a closed inbox is empty.
fn absorb_inbox(channel: &ChannelCore, state: &mut WillowChannelState) {
    if state.inbox_open {
        let mut inbox = channel
            .inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        append_inbox(state, &mut inbox);
    }
}

fn append_inbox(state: &mut WillowChannelState, inbox: &mut Inbox) {
    if inbox.values.is_empty() {
        return;
    }
    crate::observation::record(crate::observation::Event::ChannelInboxBatches, 1);
    if state.values.is_empty() {
        // Nothing to scan or pop remains in an empty deque, so its scan
        // offsets are already zero.
        std::mem::swap(&mut state.values, &mut inbox.values);
    } else {
        state.values.append(&mut inbox.values);
    }
}

/// Close the inbox before a receiver registers as a waiter, keeping every
/// value it held at the back of `values`.
fn close_inbox(channel: &ChannelCore, state: &mut WillowChannelState) {
    if state.inbox_open {
        let mut inbox = channel.inbox.lock().expect("channel inbox poisoned");
        append_inbox(state, &mut inbox);
        inbox.open = false;
        channel
            .inbox_hint
            .store(false, std::sync::atomic::Ordering::Relaxed);
        state.inbox_open = false;
    }
}

/// Reopen the inbox once a receive leaves no receive waiter on an open
/// unbounded channel.
fn open_inbox_if_idle(channel: &ChannelCore, state: &mut WillowChannelState) {
    if !state.inbox_open && !state.closed && state.capacity.is_none() && state.waiters.is_empty() {
        channel.inbox.lock().expect("channel inbox poisoned").open = true;
        channel
            .inbox_hint
            .store(true, std::sync::atomic::Ordering::Relaxed);
        state.inbox_open = true;
    }
}

/// Receive-side view of the queue: when `values` holds no unreserved value,
/// pull in the inbox batch first.
fn refill_from_inbox(channel: &ChannelCore, state: &mut WillowChannelState) {
    if state.inbox_open && state.values.len() <= state.recv_claims.len() {
        absorb_inbox(channel, state);
    }
}

fn state_is_full(state: &WillowChannelState) -> bool {
    matches!(state.capacity, Some(cap) if state.values.len() + state.send_handoffs.len() >= cap)
}

fn token(raw: *mut c_void, role: ChannelRole, generation: u64) -> ChannelOwnershipToken {
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let channel = unsafe { channel_from_raw(raw) }.expect("ownership requires a live channel");
    core_token(channel, role, generation)
}

fn core_token(channel: &ChannelCore, role: ChannelRole, generation: u64) -> ChannelOwnershipToken {
    ChannelOwnershipToken {
        channel: channel as *const ChannelCore as usize,
        role,
        generation,
    }
}

fn next_generation(state: &mut WillowChannelState) -> u64 {
    state.next_generation = state
        .next_generation
        .checked_add(1)
        .expect("channel generation exhausted");
    state.next_generation
}

fn register_wait(raw: *mut c_void, queue: &mut WaiterQueue, task: u64, role: ChannelRole) -> bool {
    if task == 0 {
        return false;
    }
    let (generation, fresh) = queue.register_ticket_fresh(task);
    if !crate::scheduler::install_channel_ownership(task, token(raw, role, generation)) {
        queue.remove_ticket(task, generation);
        return false;
    }
    fresh
}

fn clear_wait(raw: *mut c_void, queue: &mut WaiterQueue, task: u64, role: ChannelRole) {
    if let Some(generation) = queue.ticket(task) {
        queue.remove_ticket(task, generation);
        crate::scheduler::clear_channel_ownership(task, token(raw, role, generation));
    }
}

fn clear_reservation(raw: *mut c_void, owners: &mut IdMap<u64, u64>, task: u64, role: ChannelRole) {
    if let Some(generation) = owners.remove(&task) {
        crate::scheduler::clear_channel_ownership(task, token(raw, role, generation));
    }
}

/// Successful sends reserve one value for each receiver we actually wake.
/// Returns 1 when the send completed (or raised `send on closed channel`;
/// the caller's panic check then unwinds) and 0 after registering the
/// running task as a send waiter on a full bounded channel.
///
/// # Safety
/// A non-null raw pointer must identify a current live channel. The union value must have
/// the initialized representation selected by its channel; a reference word must be null or
/// the current address of a managed payload.
unsafe fn channel_try_send_value(raw: *mut c_void, value: WillowChannelValue) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 1;
    };
    // Keep only the stable native core across receiver and sender wakes.
    let channel: &ChannelCore = channel;
    if channel.is_ref {
        // Any receiver task may take the value: publish it before it is
        // queued. O(1) when the blocking send already published it.
        crate::gc::publish_in(
            crate::runtime::current(),
            // SAFETY: The channel element ABI supplies the initialized union representation read
            // by this operation.
            unsafe { value.ptr_value } as *mut u8,
            crate::gc::PublicationPoint::ChannelSend,
            false,
        );
    }
    if channel
        .inbox_hint
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        let mut inbox = channel.inbox.lock().expect("channel inbox poisoned");
        if inbox.open {
            // Open implies unbounded, unclosed and no receive waiter: there is
            // no capacity, handoff or wake decision to make.
            if channel.is_ref {
                // SAFETY: The typed send supplies the live channel and matching union member;
                // after a collecting wait both the channel and any reference value are
                // reloaded from roots.
                unsafe { channel_send_barrier(raw, value) };
            }
            inbox.values.push_back(value);
            return 1;
        }
    }
    let current = crate::scheduler::willow_sched_current_task();
    crate::task::prepare_channel_wait_links();
    let (wake_receivers, wake_senders) = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        // A sender that found the inbox closed may run after it reopened:
        // queue behind the values it holds.
        absorb_inbox(channel, &mut state);
        if state.closed {
            clear_wait(raw, &mut state.send_waiters, current, ChannelRole::SendWait);
            clear_reservation(
                raw,
                &mut state.send_handoffs,
                current,
                ChannelRole::SendHandoff,
            );
            // The value is never queued: the sender panics like `recv` on a
            // closed empty channel. Raising allocates, so release the channel
            // lock and the no-preempt region first.
            drop(state);
            drop(_no_preempt);
            channel_raise_with("send on closed channel");
            return 1;
        }
        if !state.send_handoffs.contains_key(&current) && state_is_full(&state) {
            register_wait(raw, &mut state.send_waiters, current, ChannelRole::SendWait);
            return 0;
        }
        clear_wait(raw, &mut state.send_waiters, current, ChannelRole::SendWait);
        clear_reservation(
            raw,
            &mut state.send_handoffs,
            current,
            ChannelRole::SendHandoff,
        );
        if channel.is_ref {
            // SAFETY: The typed send supplies the live channel and matching union member;
            // after a collecting wait both the channel and any reference value are reloaded
            // from roots.
            unsafe { channel_send_barrier(raw, value) };
        }
        state.values.push_back(value);
        let receive =
            (!state.waiters.is_empty()).then(|| reserve_first_waiter(channel, &mut state, true));
        (receive, !state.send_waiters.is_empty())
    };
    // A waiter arriving after this snapshot probes the updated availability
    // under the same mutex. It cannot depend on this notification.
    if let Some(first) = wake_receivers {
        publish_first_waiter(channel, true, first);
    }
    if wake_senders {
        wake_send_waiters(channel);
    }
    1
}

/// The write barrier is non-safepointing; collection/wakes happen only after
/// the channel locks have been released. The channel is a GC payload start and
/// the value a published payload: with the header facts an old channel is
/// remembered once per minor (its header claim filters later sends) instead of
/// logging one unresolved edge per send and flushing them under the heap lock.
///
/// # Safety
/// A non-null raw pointer must identify a current live channel. The union value must have
/// the initialized representation selected by its channel; a reference word must be null or
/// the current address of a managed payload.
unsafe fn channel_send_barrier(raw: *mut c_void, value: WillowChannelValue) {
    // SAFETY: A live reference channel is inserting its matching payload into a fresh queue
    // position.
    unsafe {
        crate::gc::write_barrier_object_owner(
            raw as *mut u8,
            std::ptr::null_mut(),
            // SAFETY: The channel element ABI supplies the initialized union representation read by
            // this operation.
            value.ptr_value as *mut u8,
            crate::gc::GcStoreDestination::ContainerInternal as i64,
        )
    };
}

#[cfg(test)]
/// Remove a completed/cancelled task from every channel waiter queue. This is
/// needed for a task cancelled while parked on `select`: no case is chosen, so
/// generated unregister-all code never runs.
pub(crate) fn purge_task(task_id: u64) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // O(channels the task actually parked on), via the task-side reverse
    // references recorded at registration. Native core addresses
    // stay stable and live while the waiter's rooted frame retains the wrapper.
    let ownership = crate::scheduler::take_channel_waits(task_id);
    purge_task_from_tokens(task_id, ownership);
}

/// Terminal cleanup snapshots tokens while the rooted frame is still live.
/// Group roles per channel so compensation sees the complete released state.
pub(crate) fn purge_task_from_tokens(
    task_id: u64,
    ownership: impl IntoIterator<Item = ChannelOwnershipToken>,
) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let mut grouped: HashMap<usize, Vec<ChannelOwnershipToken>> = HashMap::new();
    for owner in ownership {
        grouped.entry(owner.channel).or_default().push(owner);
    }
    for (address, owners) in grouped {
        debug_assert_ne!(address, 0);
        // Frame retention keeps the owning wrapper (and its native box) live.
        // This identity never points into relocatable GC storage.
        // SAFETY: The retained ownership token keeps the channel core at its stable address during
        // cleanup.
        let channel = unsafe { &*(address as *const ChannelCore) };
        {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            for owner in owners {
                remove_exact(&mut state, task_id, owner);
            }
        }
        wake_recv_waiters(channel);
        wake_send_waiters(channel);
    }
}

fn remove_exact(state: &mut WillowChannelState, task: u64, owner: ChannelOwnershipToken) {
    match owner.role {
        ChannelRole::RecvWait => {
            state.waiters.remove_ticket(task, owner.generation);
        }
        ChannelRole::SendWait => {
            state.send_waiters.remove_ticket(task, owner.generation);
        }
        ChannelRole::RecvClaim | ChannelRole::SendHandoff => {
            let map = if owner.role == ChannelRole::RecvClaim {
                &mut state.recv_claims
            } else {
                &mut state.send_handoffs
            };
            if map.get(&task) == Some(&owner.generation) {
                map.remove(&task);
            }
        }
    }
    crate::scheduler::clear_channel_ownership(task, owner);
}

/// # Safety
///
/// `raw` must name a live, initialized `WillowAbiChannel` GC payload and the
/// owning handle must remain rooted for the returned borrow.
unsafe fn channel_from_raw<'a>(raw: *mut c_void) -> Option<&'a WillowAbiChannel> {
    if raw.is_null() {
        None
    } else {
        // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
        Some(unsafe { &*(raw as *mut WillowAbiChannel) })
    }
}

///
/// # Safety
/// A non-null raw pointer must identify a current live channel. The union value must have
/// the initialized representation selected by its channel; a reference word must be null or
/// the current address of a managed payload.
unsafe fn willow_channel_send_value(raw: *mut c_void, value: WillowChannelValue) {
    // A blocking send drives the scheduler (a GC point), so a large graph
    // may park mid-publication here.
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let is_ref = unsafe { channel_from_raw(raw) }.is_some_and(|channel| channel.is_ref);
    let (raw, value) = if is_ref {
        // SAFETY: A reference channel carries the pointer member of the ABI union.
        let [channel, message] =
            crate::gc::publish_channel_send(raw.cast(), unsafe { value.ptr_value.cast() });
        (
            channel.cast(),
            WillowChannelValue {
                ptr_value: message.cast(),
            },
        )
    } else {
        (raw, value)
    };
    // Fast path + unbounded path: try once and assign receiver claims.
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    if (unsafe { channel_try_send_value(raw, value) }) != 0 {
        return;
    }
    // A minor collection inside the drives below may move the channel
    // wrapper and a reference value (shared or not): root both and reload
    // them after every drive. Only this blocking slow path pays for it.
    let raw_root = crate::gc::GcRootHandle::new(raw.cast());
    let value_root =
        // SAFETY: The channel element ABI supplies the initialized union representation read by
        // this operation.
        is_ref.then(|| crate::gc::GcRootHandle::new(unsafe { value.ptr_value }.cast()));
    #[cfg(feature = "deterministic-test-clock")]
    crate::test_clock::sync_send_wait();
    // Bounded channel is FULL in a synchronous context: help drive the
    // scheduler so consumers can free space (mirror of sync recv). If no
    // task can progress and the buffer is still full/open, abort with a
    // clear runtime panic instead of deadlocking.
    loop {
        // Re-probe this channel even if unrelated tasks remain runnable.
        // An unbounded nested drive can wait for a task that is itself waiting
        // for this send/recv to return, despite the channel already being ready.
        let completed = crate::scheduler::willow_sched_run_until_deadline(
            crate::scheduler::willow_monotonic_millis().saturating_add(1),
        );
        let raw: *mut c_void = raw_root.load().cast();
        let value = value_root
            .as_ref()
            .map_or(value, |root| WillowChannelValue {
                ptr_value: root.load().cast(),
            });
        // SAFETY: The typed send supplies the live channel and matching union member; after a
        // collecting wait both the channel and any reference value are reloaded from roots.
        if (unsafe { channel_try_send_value(raw, value) }) != 0 {
            // The failed attempts registered this task as a send waiter (a
            // no-op outside a task). Drop that registration: nobody will
            // consume the wake, and a stale entry costs a spurious wakeup.
            // A successful send can wake a task and collect under scheduler stress.
            // SAFETY: The handle is null or the live channel retained for this operation.
            unsafe { willow_channel_unregister_waiter(raw_root.load().cast()) };
            return;
        }
        if completed == 0 {
            // "No task completed" is not proof that nothing can ever happen: a
            // drive can race a claim or stop on a timer boundary. Only an
            // absent wake source means this send would block forever.
            //
            if crate::scheduler::scheduler_has_wake_source() {
                crate::scheduler::wait_for_any_wake_briefly();
                continue;
            }
            // SAFETY: The handle is null or the live channel retained for this operation.
            unsafe { willow_channel_unregister_waiter(raw) };
            channel_raise_with("send on full bounded channel would block");
            return;
        }
    }
}

const CHANNEL_WAKE_BATCH: usize = 32;
type WakeCandidate = (u64, ChannelOwnershipToken);

/// Reserve at most one bounded batch under Channel -> TaskShard lock order.
/// The last flag means availability or the waiter queue was exhausted.
fn reserve_waiter_batch<const N: usize>(
    channel: &ChannelCore,
    receive: bool,
    candidates: &mut [WakeCandidate; N],
) -> (usize, bool, bool) {
    crate::observation::record(crate::observation::Event::ChannelReserveLocks, 1);
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    reserve_waiter_batch_locked(channel, &mut state, receive, candidates)
}

fn reserve_waiter_batch_locked<const N: usize>(
    channel: &ChannelCore,
    state: &mut WillowChannelState,
    receive: bool,
    candidates: &mut [WakeCandidate; N],
) -> (usize, bool, bool) {
    let mut count = 0;
    for _ in 0..N {
        if if receive {
            state.values.len() <= state.recv_claims.len()
        } else {
            state.closed || state_is_full(state)
        } {
            return (count, state.closed, true);
        }
        let queue = if receive {
            &mut state.waiters
        } else {
            &mut state.send_waiters
        };
        let Some((task, wait)) = queue.pop_front_ticket() else {
            return (count, state.closed, true);
        };
        crate::observation::record(crate::observation::Event::ChannelWaiterVisits, 1);
        let generation = next_generation(state);
        let (wait_role, owned_role) = if receive {
            (ChannelRole::RecvWait, ChannelRole::RecvClaim)
        } else {
            (ChannelRole::SendWait, ChannelRole::SendHandoff)
        };
        let old = core_token(channel, wait_role, wait);
        let owner = core_token(channel, owned_role, generation);
        let map = if receive {
            &mut state.recv_claims
        } else {
            &mut state.send_handoffs
        };
        debug_assert!(!map.contains_key(&task));
        map.insert(task, generation);
        if crate::scheduler::transition_channel_ownership(task, old, owner) {
            candidates[count] = (task, owner);
            count += 1;
        } else {
            map.remove(&task);
            crate::scheduler::clear_channel_ownership(task, old);
        }
    }
    let exhausted = if receive {
        state.values.len() <= state.recv_claims.len() || state.waiters.is_empty()
    } else {
        state.closed || state_is_full(state) || state.send_waiters.is_empty()
    };
    (count, state.closed, exhausted)
}

/// Publish outside the channel lock, then undo only failed generations.
/// Return whether released reservations may let another waiter make progress.
fn publish_waiter_batch(
    channel: &ChannelCore,
    candidates: &[WakeCandidate],
    scratch: &mut crate::scheduler::WakeBatchScratch,
) -> bool {
    crate::observation::record(
        crate::observation::Event::ChannelWakeAttempts,
        candidates.len(),
    );
    if candidates.is_empty() {
        return false;
    }
    crate::observation::record(crate::observation::Event::ChannelWakeBatches, 1);
    // Keep the common ping-pong path allocation-free, without shard partitioning.
    if let [(task, owner)] = candidates {
        if crate::scheduler::wake_channel_owner(*task) {
            return false;
        }
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        remove_exact(&mut state, *task, *owner);
        return true;
    }
    let mut ids = [0; CHANNEL_WAKE_BATCH];
    for (id, (task, _)) in ids.iter_mut().zip(candidates) {
        *id = *task;
    }
    crate::scheduler::wake_channel_owners(&ids[..candidates.len()], scratch);
    if scratch.terminal.is_empty() {
        return false;
    }
    // Scheduler results are shard-ordered. Index failures rather than rescanning
    // candidates for each terminal task; allocate only on this uncommon path.
    let terminal: HashSet<_> = scratch.terminal.iter().copied().collect();
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    for &(task, owner) in candidates {
        if terminal.contains(&task) {
            remove_exact(&mut state, task, owner);
        }
    }
    true
}

fn wake_reserved_waiters(channel: &ChannelCore, receive: bool) {
    // Most notifications release one value/slot. Avoid initializing full batch
    // storage on this path; a remaining fanout uses bounded batches below.
    let mut first = [(0, core_token(channel, ChannelRole::RecvClaim, 0))];
    let (count, closed, exhausted) = reserve_waiter_batch(channel, receive, &mut first);
    publish_first_waiter(channel, receive, (first, count, closed, exhausted));
}

type FirstWaiter = ([WakeCandidate; 1], usize, bool, bool);

fn reserve_first_waiter(
    channel: &ChannelCore,
    state: &mut WillowChannelState,
    receive: bool,
) -> FirstWaiter {
    let mut first = [(0, core_token(channel, ChannelRole::RecvClaim, 0))];
    let (count, closed, exhausted) =
        reserve_waiter_batch_locked(channel, state, receive, &mut first);
    (first, count, closed, exhausted)
}

fn publish_first_waiter(channel: &ChannelCore, receive: bool, first: FirstWaiter) {
    let (first, count, closed, exhausted) = first;
    let released = publish_waiter_batch(channel, &first[..count], &mut Default::default());
    if exhausted && !released {
        if receive && closed {
            wake_closed_empty(channel);
        }
        return;
    }
    wake_reserved_batches(channel, receive);
}

fn publish_first_waiter_bounded(
    raw: *mut c_void,
    channel: &ChannelCore,
    receive: bool,
    first: FirstWaiter,
) -> *mut c_void {
    let (first, count, closed, exhausted) = first;
    // Publication is non-safepointing except for the explicit scheduler stress
    // hook. Keep its optional relocation visible before creating a deferred job.
    let root = (count != 0 && crate::gc::gc_stress_enabled("scheduler"))
        .then(|| crate::gc::GcRootHandle::new(raw.cast()));
    let released = publish_waiter_batch(channel, &first[..count], &mut Default::default());
    let raw = root.as_ref().map_or(raw, |root| root.load().cast());
    if !exhausted || released || (receive && closed) {
        return deferred::schedule(raw, channel, receive);
    }
    raw
}

fn wake_waiters_bounded(raw: *mut c_void, channel: &ChannelCore, receive: bool) -> *mut c_void {
    let mut first = [(0, core_token(channel, ChannelRole::RecvClaim, 0))];
    let (count, closed, exhausted) = reserve_waiter_batch(channel, receive, &mut first);
    publish_first_waiter_bounded(raw, channel, receive, (first, count, closed, exhausted))
}

// Keep large scratch setup out of the frequent zero/single-waiter call frame.
#[inline(never)]
fn wake_reserved_batches(channel: &ChannelCore, receive: bool) {
    let mut candidates = [(0, core_token(channel, ChannelRole::RecvClaim, 0)); CHANNEL_WAKE_BATCH];
    let mut scratch = crate::scheduler::WakeBatchScratch::default();
    loop {
        let (count, closed, exhausted) = reserve_waiter_batch(channel, receive, &mut candidates);
        let released = publish_waiter_batch(channel, &candidates[..count], &mut scratch);
        if exhausted && !released {
            if receive && closed {
                wake_closed_empty(channel);
            }
            break;
        }
    }
}

#[cfg(test)]
const CHANNEL_WAKE_ATTEMPTS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelWakeAttempts);

#[cfg(test)]
const CHANNEL_WAKE_BATCHES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelWakeBatches);

#[cfg(test)]
const CHANNEL_RESERVE_LOCKS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelReserveLocks);

#[cfg(test)]
const CHANNEL_INBOX_BATCHES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelInboxBatches);

#[cfg(test)]
const CHANNEL_WAITER_VISITS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelWaiterVisits);

fn wake_recv_waiters(channel: &ChannelCore) {
    wake_reserved_waiters(channel, true);
}
fn wake_send_waiters(channel: &ChannelCore) {
    wake_reserved_waiters(channel, false);
}

fn drain_waiters(
    channel: &ChannelCore,
    queue: &mut WaiterQueue,
    role: ChannelRole,
    wake: &mut HashSet<u64>,
) {
    while let Some((task, generation)) = queue.pop_front_ticket() {
        crate::scheduler::clear_channel_ownership(task, core_token(channel, role, generation));
        wake.insert(task);
    }
}

fn wake_closed_empty(channel: &ChannelCore) {
    let mut wake = HashSet::new();
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if !state.closed || !state.values.is_empty() {
            return;
        }
        drain_waiters(
            channel,
            &mut state.waiters,
            ChannelRole::RecvWait,
            &mut wake,
        );
    }
    let wake: Vec<_> = wake.into_iter().collect();
    // The native Vec remains valid across scheduler GC stress points.
    // SAFETY: The slice supplies exactly the indicated number of task IDs and remains live for the
    // call.
    unsafe { crate::scheduler::willow_sched_wake_many(wake.as_ptr(), wake.len()) };
}

fn take_value(
    raw: *mut c_void,
    state: &mut WillowChannelState,
    current: u64,
) -> Option<WillowChannelValue> {
    if state.recv_claims.contains_key(&current) || state.values.len() > state.recv_claims.len() {
        clear_reservation(raw, &mut state.recv_claims, current, ChannelRole::RecvClaim);
        clear_wait(raw, &mut state.waiters, current, ChannelRole::RecvWait);
        if let Some(value) = state.values.front()
            // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
            && unsafe { channel_from_raw(raw) }.is_some_and(|channel| channel.is_ref)
        {
            // Snapshot the logical old edge while the queue lock still owns it.
            // SAFETY: The channel element ABI supplies the initialized union representation read
            // by this operation.
            crate::gc::satb_delete(unsafe { value.ptr_value }.cast());
        }
        let value = state.values.pop_front();
        if value.is_some() {
            if state.gc_scan_position != 0 {
                state.gc_scan_position -= 1;
            } else {
                state.gc_scan_remaining = state.gc_scan_remaining.saturating_sub(1);
            }
        }
        value
    } else {
        None
    }
}

/// After `take_value`: release the queue lock, then hand the freed capacity to
/// a blocked sender and pass a remaining value on to another receiver. Runs
/// with preemption disabled by the caller.
fn finish_take(
    raw: *mut c_void,
    channel: &ChannelCore,
    mut state: crate::native_memory::MutexGuard<'_, WillowChannelState>,
    mut value: WillowChannelValue,
) -> WillowChannelValue {
    let wake_senders =
        (!state.send_waiters.is_empty()).then(|| reserve_first_waiter(channel, &mut state, false));
    let wake_receivers = !state.waiters.is_empty();
    drop(state);
    if wake_senders.is_none() && !wake_receivers {
        return value;
    }
    // Waking can explicitly collect under scheduler GC stress.
    // The popped reference no longer has the queue as its root.
    let root = channel
        .is_ref
        // SAFETY: The channel element ABI supplies the initialized union representation read by
        // this operation.
        .then(|| crate::gc::GcRootHandle::new(unsafe { value.ptr_value.cast() }));
    let raw = if let Some(first) = wake_senders {
        publish_first_waiter_bounded(raw, channel, false, first)
    } else {
        raw
    };
    if wake_receivers {
        wake_waiters_bounded(raw, channel, true);
    }
    if let Some(root) = root {
        value.ptr_value = root.load().cast();
    }
    value
}

fn willow_channel_recv_value(raw: *mut c_void) -> WillowChannelValue {
    match channel_recv_or_closed(raw) {
        Some(value) => value,
        None => {
            channel_raise_with("recv on closed empty channel");
            WillowChannelValue::default()
        }
    }
}

/// Receive the next value, blocking while the channel is empty and open.
/// `None` means the channel is closed AND drained (`Channel<T>::recv_opt()`,
/// close-aware receive); `recv()` turns that into its closed-empty panic. A null
/// channel also answers `None`. Other failures (a wait that can never be
/// satisfied) still raise.
fn channel_recv_or_closed(raw: *mut c_void) -> Option<WillowChannelValue> {
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let channel: &ChannelCore = unsafe { channel_from_raw(raw) }?;
    let current = crate::scheduler::willow_sched_current_task();
    match try_recv_or_closed(raw, channel, current) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => wait_for_value(raw, channel, current),
    }
}

fn try_recv_or_closed(
    raw: *mut c_void,
    channel: &ChannelCore,
    current: u64,
) -> std::task::Poll<Option<WillowChannelValue>> {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    refill_from_inbox(channel, &mut state);
    if let Some(value) = take_value(raw, &mut state, current) {
        open_inbox_if_idle(channel, &mut state);
        return std::task::Poll::Ready(Some(finish_take(raw, channel, state, value)));
    }
    if state.closed && state.values.is_empty() {
        return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
}

// Only the blocking path needs a root. The wrapper can move during a scheduler
// drive, while its boxed core stays at the same address until the root drops.
#[inline(never)]
fn wait_for_value(
    raw: *mut c_void,
    channel: &ChannelCore,
    current: u64,
) -> Option<WillowChannelValue> {
    let root = crate::gc::GcRootHandle::new(raw.cast());
    #[cfg(feature = "deterministic-test-clock")]
    crate::test_clock::sync_recv_wait();
    loop {
        let completed = crate::scheduler::willow_sched_run_until_deadline(
            crate::scheduler::willow_monotonic_millis().saturating_add(1),
        );
        if let std::task::Poll::Ready(result) =
            try_recv_or_closed(root.load().cast(), channel, current)
        {
            return result;
        }
        if completed == 0 {
            if !crate::scheduler::scheduler_has_wake_source() {
                channel_raise_with("recv on empty open channel would block");
                return Some(WillowChannelValue::default());
            }
            crate::scheduler::wait_for_any_wake_briefly();
        }
    }
}

fn channel_raise_with(message: &str) {
    crate::panic_context::raise_language_message(message);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    Closed,
    Empty,
}

#[derive(Debug, Clone)]
pub struct RuntimeChannel<T> {
    buffer: VecDeque<T>,
    closed: bool,
    element_type_id: i64,
}

impl<T> RuntimeChannel<T> {
    pub fn new(element_type_id: i64) -> Self {
        Self {
            buffer: VecDeque::new(),
            closed: false,
            element_type_id,
        }
    }

    pub fn element_type_id(&self) -> i64 {
        self.element_type_id
    }

    pub fn send(&mut self, value: T) -> Result<(), ChannelError> {
        if self.closed {
            return Err(ChannelError::Closed);
        }
        self.buffer.push_back(value);
        Ok(())
    }

    pub fn recv(&mut self) -> Result<T, ChannelError> {
        self.buffer.pop_front().ok_or(ChannelError::Empty)
    }

    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

impl<T: GcTrace> GcTrace for RuntimeChannel<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        for value in &self.buffer {
            value.trace(visitor);
        }
    }
}

#[cfg(test)]
#[path = "channel_inbox_tests.rs"]
mod inbox_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "channel_batch_tests.rs"]
mod batch_tests;
