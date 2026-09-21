//! Channel state, GC integration, ownership cleanup, and wake dispatch.

mod abi;
pub use abi::*;

use crate::task::{ChannelOwnershipToken, ChannelRole};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::{Condvar, Mutex};

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

/// A channel's parked-task queue: FIFO order plus expected O(1) membership
/// (willow-ezs.1.2). The implementation now lives in `crate::wait_queue` so
/// task-completion waits share exactly the same structure (willow-ezs.2);
/// channel wake behavior is unchanged.
type WaiterQueue = crate::wait_queue::WaitQueue<u64>;

#[derive(Default)]
struct WillowChannelState {
    values: VecDeque<WillowChannelValue>,
    /// Finite major snapshot: pops adjust the offset into the current deque;
    /// appends do not extend remaining work. No lifetime sequence can wrap.
    gc_scan_position: usize,
    gc_scan_remaining: usize,
    closed: bool,
    /// Cooperative consumers parked on an empty `recv`, woken FIFO by `send` /
    /// `close` (willow-dsw).
    waiters: WaiterQueue,
    /// Bounded capacity (`Channel<T>::with_capacity(n)`); `None` = unbounded
    /// (willow-o038).
    capacity: Option<usize>,
    /// Cooperative producers parked on a FULL bounded channel. A `recv` that
    /// frees one slot wakes one live producer; `close` wakes all.
    send_waiters: WaiterQueue,
    // Values remain in the traced queue; claims reserve availability, not slots
    // in movable task frames. Reservations cannot outnumber queued values.
    recv_claims: HashMap<u64, u64>,
    send_handoffs: HashMap<u64, u64>,
    next_generation: u64,
}

pub struct WillowAbiChannel {
    // Native synchronization and cancellation identity stay at a stable address
    // when the GC-owned wrapper is relocated. The rooted frame retains ownership.
    inner: Box<ChannelCore>,
}

pub struct ChannelCore {
    state: Mutex<WillowChannelState>,
    not_empty: Condvar,
    /// True when the element type is a GC reference (String / class / array /
    /// ...): queued values are then GC roots scanned by the collector
    /// (willow-dsw GC tracing).
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
                state: Mutex::new(WillowChannelState::default()),
                not_empty: Condvar::new(),
                is_ref,
            }),
        }
    }
}

/// GC type id for channel objects (willow-p4er): channels are GC-MANAGED —
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
}

unsafe fn snapshot_channel(payload: *mut u8, children: &mut Vec<*mut u8>) {
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
                .map(|value| unsafe { value.ptr_value as *mut u8 }),
        );
    }
}

unsafe fn snapshot_channel_slice(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    use crate::gc::TraceSliceProgress;
    let channel = unsafe { &*payload.cast::<WillowAbiChannel>() };
    if !channel.is_ref {
        return TraceSliceProgress::Done;
    }
    let mut state = match channel.state.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
    };
    if cursor == 0 {
        state.gc_scan_position = 0;
        state.gc_scan_remaining = state.values.len();
    }
    let count = state.gc_scan_remaining.min(limit);
    let end = state.gc_scan_position + count;
    for index in state.gc_scan_position..end {
        // Indexed VecDeque access is O(1), including a wrapped backing buffer.
        children.push(unsafe { state.values[index].ptr_value }.cast());
    }
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
    unsafe {
        std::ptr::drop_in_place(payload as *mut WillowAbiChannel);
    }
    #[cfg(test)]
    CHANNEL_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
static CHANNEL_DROP_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static CHANNEL_REGISTRATION: crate::gc::NativeGcRegistration =
    crate::gc::NativeGcRegistration::new();
const CHANNEL_GC_TYPES: &[crate::gc::NativeGcType] =
    &[
        crate::gc::NativeGcType::new(CHANNEL_TYPE_ID, Some(trace_channel), Some(drop_channel))
            .with_concurrent_trace(snapshot_channel)
            .with_concurrent_slice(snapshot_channel_slice),
    ];

#[cfg(test)]
static CHANNEL_REGISTRATION_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Register both GC hooks once per registry generation. The common channel
/// allocation path performs only two atomic loads; the mutex and registry
/// HashMap locks are taken only after a GC reset/unregister invalidates hooks.
fn ensure_channel_registered() {
    if CHANNEL_REGISTRATION.ensure(CHANNEL_GC_TYPES) {
        #[cfg(test)]
        CHANNEL_REGISTRATION_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn state_is_full(state: &WillowChannelState) -> bool {
    matches!(state.capacity, Some(cap) if state.values.len() + state.send_handoffs.len() >= cap)
}

fn token(raw: *mut c_void, role: ChannelRole, generation: u64) -> ChannelOwnershipToken {
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
    let fresh = queue.ticket(task).is_none();
    let generation = queue
        .register_ticket(task)
        .expect("wait queue generation exhausted");
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

fn clear_reservation(
    raw: *mut c_void,
    owners: &mut HashMap<u64, u64>,
    task: u64,
    role: ChannelRole,
) {
    if let Some(generation) = owners.remove(&task) {
        crate::scheduler::clear_channel_ownership(task, token(raw, role, generation));
    }
}

/// Successful sends reserve one value for each receiver we actually wake.
fn channel_try_send_value(raw: *mut c_void, value: WillowChannelValue) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 1;
    };
    let current = crate::scheduler::willow_sched_current_task();
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if state.closed {
            clear_wait(raw, &mut state.send_waiters, current, ChannelRole::SendWait);
            clear_reservation(
                raw,
                &mut state.send_handoffs,
                current,
                ChannelRole::SendHandoff,
            );
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
            // The write barrier is non-safepointing; collection/wakes happen
            // only after this channel lock has been released.
            crate::gc::willow_gc_write_barrier(
                raw as *mut u8,
                std::ptr::null_mut(),
                unsafe { value.ptr_value } as *mut u8,
                crate::gc::GcStoreDestination::ContainerInternal as i64,
            );
        }
        state.values.push_back(value);
        channel.not_empty.notify_one();
    }
    wake_recv_waiters(channel);
    wake_send_waiters(channel);
    1
}

/// Remove a completed/cancelled task from every channel waiter queue. This is
/// needed for a task cancelled while parked on `select`: no case is chosen, so
/// generated unregister-all code never runs.
pub(crate) fn purge_task(task_id: u64) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // O(channels the task actually parked on), via the task-side reverse
    // references recorded at registration (willow-p4er). Native core addresses
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
        Some(unsafe { &*(raw as *mut WillowAbiChannel) })
    }
}

fn willow_channel_send_value(raw: *mut c_void, value: WillowChannelValue) {
    // Fast path + unbounded path: try once and assign receiver claims.
    if channel_try_send_value(raw, value) != 0 {
        return;
    }
    // Bounded channel is FULL in a synchronous context: help drive the
    // scheduler so consumers can free space (mirror of sync recv). If no
    // task can progress and the buffer is still full/open, abort with a
    // clear runtime panic instead of deadlocking (willow-o038).
    loop {
        // Re-probe this channel even if unrelated tasks remain runnable.
        // An unbounded nested drive can wait for a task that is itself waiting
        // for this send/recv to return, despite the channel already being ready.
        let completed = crate::scheduler::willow_sched_run_until_deadline(
            crate::scheduler::willow_monotonic_millis().saturating_add(1),
        );
        if channel_try_send_value(raw, value) != 0 {
            // The failed attempts registered this task as a send waiter (a
            // no-op outside a task). Drop that registration: nobody will
            // consume the wake, and a stale entry costs a spurious wakeup.
            willow_channel_unregister_waiter(raw);
            return;
        }
        if completed == 0 {
            // "No task completed" is not proof that nothing can ever happen: a
            // drive can race a claim or stop on a timer boundary. Only an
            // absent wake source means this send would block forever
            // (willow-atth).
            if crate::scheduler::scheduler_has_wake_source() {
                crate::scheduler::wait_for_any_wake_briefly();
                continue;
            }
            willow_channel_unregister_waiter(raw);
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
    #[cfg(test)]
    CHANNEL_RESERVE_LOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    let mut count = 0;
    for _ in 0..N {
        if if receive {
            state.values.len() <= state.recv_claims.len()
        } else {
            state.closed || state_is_full(&state)
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
        let generation = next_generation(&mut state);
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
        state.closed || state_is_full(&state) || state.send_waiters.is_empty()
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
    #[cfg(test)]
    CHANNEL_WAKE_ATTEMPTS.fetch_add(candidates.len(), std::sync::atomic::Ordering::Relaxed);
    if candidates.is_empty() {
        return false;
    }
    #[cfg(test)]
    CHANNEL_WAKE_BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    let released = publish_waiter_batch(channel, &first[..count], &mut Default::default());
    if exhausted && !released {
        if receive && closed {
            wake_closed_empty(channel);
        }
        return;
    }
    wake_reserved_batches(channel, receive);
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
static CHANNEL_WAKE_ATTEMPTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static CHANNEL_WAKE_BATCHES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static CHANNEL_RESERVE_LOCKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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
            && unsafe { channel_from_raw(raw) }.is_some_and(|channel| channel.is_ref)
        {
            // Snapshot the logical old edge while the queue lock still owns it.
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

fn willow_channel_recv_value(raw: *mut c_void) -> WillowChannelValue {
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return WillowChannelValue::default();
    };
    let current = crate::scheduler::willow_sched_current_task();
    let mut no_progress = false;
    loop {
        {
            let _no_preempt = crate::preempt::NoPreemptGuard::enter();
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            if let Some(mut value) = take_value(raw, &mut state, current) {
                drop(state);
                // Waking can explicitly collect under scheduler GC stress.
                // The popped reference no longer has the queue as its root.
                if channel.is_ref {
                    crate::gc::willow_push_root(std::ptr::addr_of_mut!(value.ptr_value).cast());
                }
                wake_send_waiters(channel);
                wake_recv_waiters(channel);
                if channel.is_ref {
                    crate::gc::willow_pop_roots(1);
                }
                return value;
            }
            if state.closed && state.values.is_empty() {
                drop(state);
                channel_raise_with("recv on closed empty channel");
                return WillowChannelValue::default();
            }
        }
        if no_progress {
            if crate::scheduler::scheduler_has_wake_source() {
                crate::scheduler::wait_for_any_wake_briefly();
            } else {
                channel_raise_with("recv on empty open channel would block");
                return WillowChannelValue::default();
            }
        }
        no_progress = crate::scheduler::willow_sched_run_until_deadline(
            crate::scheduler::willow_monotonic_millis().saturating_add(1),
        ) == 0;
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
mod tests;

#[cfg(test)]
#[path = "channel_batch_tests.rs"]
mod batch_tests;
