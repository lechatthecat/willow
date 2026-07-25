use std::collections::VecDeque;
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
}

pub struct WillowAbiChannel {
    state: Mutex<WillowChannelState>,
    not_empty: Condvar,
    /// True when the element type is a GC reference (String / class / array /
    /// ...): queued values are then GC roots scanned by the collector
    /// (willow-dsw GC tracing).
    is_ref: bool,
}

impl WillowAbiChannel {
    fn new(is_ref: bool) -> Self {
        Self {
            state: Mutex::new(WillowChannelState::default()),
            not_empty: Condvar::new(),
            is_ref,
        }
    }
}

/// GC type id for channel objects (willow-p4er): channels are GC-MANAGED —
/// unreachable channels are reclaimed by the collector like any object, and
/// their queued reference values are traced by [`trace_channel`]. The old
/// program-lifetime leak + global registry (and its O(all-channels)
/// cancellation scan) are gone; cancellation uses task-side reverse
/// references instead.
const CHANNEL_TYPE_ID: u32 = 0xC4A2_0001;

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
    if let Ok(mut state) = channel.state.lock() {
        for value in &mut state.values {
            slots.push(std::ptr::addr_of_mut!(value.ptr_value).cast::<*mut u8>());
        }
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

/// Generation in which the channel hooks were most recently installed.
/// Generation 0 is reserved as the never-registered sentinel.
static CHANNEL_REGISTERED_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static CHANNEL_REGISTRATION_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
static CHANNEL_REGISTRATION_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Register both GC hooks once per registry generation. The common channel
/// allocation path performs only two atomic loads; the mutex and registry
/// HashMap locks are taken only after a GC reset/unregister invalidates hooks.
fn ensure_channel_registered() {
    let generation = crate::gc::registry_generation();
    if CHANNEL_REGISTERED_GENERATION.load(std::sync::atomic::Ordering::Acquire) == generation {
        return;
    }

    let _registration = CHANNEL_REGISTRATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = crate::gc::registry_generation();
    if CHANNEL_REGISTERED_GENERATION.load(std::sync::atomic::Ordering::Acquire) == generation {
        return;
    }

    crate::gc::willow_register_type(CHANNEL_TYPE_ID, trace_channel);
    crate::gc::willow_register_drop(CHANNEL_TYPE_ID, drop_channel);
    CHANNEL_REGISTERED_GENERATION.store(generation, std::sync::atomic::Ordering::Release);
    #[cfg(test)]
    CHANNEL_REGISTRATION_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_new(is_ref: i64) -> *mut c_void {
    ensure_channel_registered();
    let is_ref = is_ref != 0;
    let payload = crate::gc::willow_alloc_with_layout(
        crate::gc::GcObjectKind::Channel,
        CHANNEL_TYPE_ID,
        std::mem::size_of::<WillowAbiChannel>() as i64,
        0,
    );
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    // Placement-init into GC memory. `drop_channel` runs the Rust destructor
    // during sweep so the state-owned queue buffers are released too.
    unsafe {
        (payload as *mut WillowAbiChannel).write(WillowAbiChannel::new(is_ref));
    }
    payload as *mut c_void
}

/// `Channel<T>::with_capacity(n)` (willow-o038): a BOUNDED channel — `send`
/// on a full buffer parks the producer until a `recv` frees space or the
/// channel closes. Capacity must be positive; rendezvous (capacity 0) is
/// explicitly unsupported in v1.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_new_bounded(is_ref: i64, capacity: i64) -> *mut c_void {
    if capacity <= 0 {
        channel_abort_with(
            "channel capacity must be positive (rendezvous channels are not supported)",
        );
    }
    let raw = willow_channel_new(is_ref);
    if let Some(channel) = channel_from_raw(raw) {
        channel
            .state
            .lock()
            .expect("channel mutex poisoned")
            .capacity = Some(capacity as usize);
    }
    raw
}

fn state_is_full(state: &WillowChannelState) -> bool {
    matches!(state.capacity, Some(cap) if state.values.len() >= cap)
}

/// Try to send without blocking (willow-o038): returns 1 when the value was
/// enqueued OR the channel is closed (send-on-closed is a documented no-op);
/// returns 0 when the bounded buffer is FULL — after registering the
/// currently-running task as a SEND waiter (mirror of recv_ready), so a
/// later `recv`/`close` wakes it.
fn channel_try_send_value(raw: *mut c_void, value: WillowChannelValue) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = channel_from_raw(raw) else {
        return 1;
    };
    let current = crate::scheduler::willow_sched_current_task();
    let (sent, waiters, clear_current_wait, wake_another_sender): (bool, Vec<u64>, bool, bool) = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if state.closed {
            (true, Vec::new(), current != 0, false)
        } else if state_is_full(&state) {
            let registered = current != 0 && state.send_waiters.register(current);
            if registered {
                // Registration and its reverse reference are one logical
                // operation with respect to channel wake/close. The runtime's
                // lock order is Channel -> Scheduler; scheduler code must
                // never acquire a channel lock while holding its own lock.
                crate::scheduler::record_channel_wait(current, raw as usize);
            }
            return 0;
        } else {
            // A producer woken by `wake_send_waiters` keeps its task-side
            // channel reference until it either sends, unregisters after
            // selecting another case, or is terminally purged. Clear a queued
            // registration too if this task became runnable through another
            // select arm before its send waiter was popped.
            if current != 0 {
                state.send_waiters.remove(current);
            }
            if channel.is_ref {
                crate::gc::willow_gc_write_barrier(
                    raw as *mut u8,
                    unsafe { value.ptr_value } as *mut u8,
                    crate::gc::GcStoreDestination::ContainerInternal as i64,
                );
            }
            state.values.push_back(value);
            channel.not_empty.notify_one();
            let wake_another_sender = !state_is_full(&state) && !state.send_waiters.is_empty();
            (
                true,
                state.waiters.drain_all(),
                current != 0,
                wake_another_sender,
            )
        }
    };
    if clear_current_wait {
        crate::scheduler::remove_channel_wait(current, raw as usize);
    }
    for id in waiters {
        crate::scheduler::remove_channel_wait(id, raw as usize);
        crate::scheduler::willow_sched_wake(id);
    }
    if wake_another_sender {
        wake_send_waiters(raw, channel);
    }
    i32::from(sent)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_try_send_i64(raw: *mut c_void, value: i64) -> i32 {
    channel_try_send_value(raw, WillowChannelValue { i64_value: value })
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_try_send_bool(raw: *mut c_void, value: u8) -> i32 {
    channel_try_send_value(raw, WillowChannelValue { bool_value: value })
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_try_send_f64(raw: *mut c_void, value: f64) -> i32 {
    channel_try_send_value(raw, WillowChannelValue { f64_value: value })
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_try_send_ptr(raw: *mut c_void, value: *mut c_void) -> i32 {
    channel_try_send_value(raw, WillowChannelValue { ptr_value: value })
}

/// Send-readiness probe for select send cases (willow-o038): 1 when not full
/// (or closed/unbounded); 0 after registering the running task as a send
/// waiter on a FULL bounded channel.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_send_ready(raw: *mut c_void) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = channel_from_raw(raw) else {
        return 1;
    };
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    if state.closed || !state_is_full(&state) {
        return 1;
    }
    let current = crate::scheduler::willow_sched_current_task();
    if current != 0 && state.send_waiters.register(current) {
        // Keep the channel lock until the task-side reverse reference exists:
        // otherwise a recv/close can remove the waiter in between and leave a
        // stale channel address on the task.
        crate::scheduler::record_channel_wait(current, raw as usize);
    }
    0
}

/// Remove a completed/cancelled task from every channel waiter queue. This is
/// needed for a task cancelled while parked on `select`: no case is chosen, so
/// generated unregister-all code never runs.
pub(crate) fn purge_task(task_id: u64) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // O(channels the task actually parked on), via the task-side reverse
    // references recorded at registration (willow-p4er). The addresses are
    // guaranteed live: a waiter's rooted frame holds the channel handle.
    let addresses = crate::scheduler::take_channel_waits(task_id);
    purge_task_from_addresses(task_id, addresses);
}

/// Remove a terminal task from the exact channels captured before its heavy
/// scheduler record was reaped (willow-ezs.1.4).
pub(crate) fn purge_task_from_addresses(task_id: u64, addresses: impl IntoIterator<Item = usize>) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    for address in addresses {
        debug_assert_ne!(address, 0, "captured channel address must be non-null");
        // SAFETY: `finish_terminal` captures these addresses while the task's
        // async frame is still a GC runtime root. `drain_terminal_cleanups`
        // invokes this before any post-poll stress collection, and
        // `release_pending_frame_roots` cannot unroot the frame until the
        // outermost drive has quiesced. Thus the frame-held channel handle
        // keeps this payload live for the entire dereference/compensation.
        let channel = unsafe { &*(address as *const WillowAbiChannel) };
        let compensate = {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            state.waiters.remove(task_id);
            state.send_waiters.remove(task_id);
            !state.closed && !state_is_full(&state) && !state.send_waiters.is_empty()
        };
        if compensate {
            // A producer can be cancelled after wake-one popped it but before
            // it retries the send. Pass the still-free capacity to the next
            // waiter instead of leaving the queue permanently parked.
            wake_send_waiters(address as *mut c_void, channel);
        }
    }
}

fn channel_from_raw(raw: *mut c_void) -> Option<&'static WillowAbiChannel> {
    if raw.is_null() {
        None
    } else {
        Some(unsafe { &*(raw as *mut WillowAbiChannel) })
    }
}

fn willow_channel_send_value(raw: *mut c_void, value: WillowChannelValue) {
    // Fast path + unbounded path: try once (also wakes recv waiters
    // drain-all — see channel_try_send_value re willow-vynv.1).
    if channel_try_send_value(raw, value) != 0 {
        return;
    }
    // Bounded channel is FULL in a synchronous context: help drive the
    // scheduler so consumers can free space (mirror of sync recv). If no
    // task can progress and the buffer is still full/open, abort with a
    // clear runtime panic instead of deadlocking (willow-o038).
    loop {
        let completed = crate::scheduler::willow_sched_run();
        if channel_try_send_value(raw, value) != 0 {
            // The failed attempts registered this task as a send waiter (a
            // no-op outside a task). Drop that registration: nobody will
            // consume the wake, and a stale entry costs a spurious wakeup.
            willow_channel_unregister_waiter(raw);
            return;
        }
        if completed == 0 {
            channel_abort_with("send on full bounded channel would block");
        }
    }
}

/// Cooperative `recv` readiness probe (willow-dsw): returns 1 if a value is
/// available OR the channel is closed (the caller then reads the value / default
/// via `willow_channel_recv_*`); returns 0 if the channel is empty and open,
/// after registering the currently-running task as a waiter — the caller's poll
/// fn then returns Pending and is woken by a later `send`/`close`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_recv_ready(raw: *mut c_void) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = channel_from_raw(raw) else {
        return 1;
    };
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    if !state.values.is_empty() || state.closed {
        return 1;
    }
    let current = crate::scheduler::willow_sched_current_task();
    if current != 0 && state.waiters.register(current) {
        // Reverse reference for O(registered) cancellation (willow-p4er):
        // the task records WHICH channels it parked on, so purge_task walks
        // only those. The channel stays reachable while the waiter lives
        // (its handle sits in the waiter's rooted frame).
        crate::scheduler::record_channel_wait(current, raw as usize);
    }
    drop(state);
    drop(_no_preempt);
    if current != 0 {
        crate::gc::stress_collect("scheduler");
    }
    0
}

/// Remove the currently-running task from this channel's waiter queue
/// (willow-7aj). A cooperative `select` registers itself (via recv_ready) on
/// every recv channel while waiting; once it picks a case it must unregister
/// from all of them so a later send/close does not spuriously wake the
/// already-resumed task.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_unregister_waiter(raw: *mut c_void) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = channel_from_raw(raw) else {
        return;
    };
    let current = crate::scheduler::willow_sched_current_task();
    if current == 0 {
        return;
    }
    let compensate = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        state.waiters.remove(current);
        state.send_waiters.remove(current);
        !state.closed && !state_is_full(&state) && !state.send_waiters.is_empty()
    };
    crate::scheduler::remove_channel_wait(current, raw as usize);
    if compensate {
        // A select task woken for a send slot may choose a different ready arm.
        // Hand that unconsumed capacity to the next parked producer.
        wake_send_waiters(raw, channel);
    }
}

/// Pass currently-available capacity to one live parked producer.
///
/// The successful handoff keeps the task-side channel reference until the
/// producer sends, unregisters after choosing another select arm, or is
/// terminally purged. Those exit paths compensate by calling this again if the
/// slot remains free, so wake-one cannot strand the rest of the queue.
/// Cancelled/stale entries are skipped until a task is actually transitioned
/// to Ready (willow-o038).
fn wake_send_waiters(raw: *mut c_void, channel: &WillowAbiChannel) {
    loop {
        let sender = {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            if state.closed || state_is_full(&state) {
                return;
            }
            state.send_waiters.pop_front()
        };
        let Some(id) = sender else {
            return;
        };
        if crate::scheduler::try_wake_parked_task(id) {
            // Do not clear the reverse reference yet: it is the cancellation
            // cleanup's proof that this task owns an unconsumed send handoff.
            return;
        }
        // Already runnable/terminal waiters do not own the handoff. Drop their
        // stale reverse reference and keep looking for one task we can wake.
        crate::scheduler::remove_channel_wait(id, raw as usize);
    }
}

fn willow_channel_recv_value(raw: *mut c_void) -> WillowChannelValue {
    let Some(channel) = channel_from_raw(raw) else {
        return WillowChannelValue::default();
    };
    // Cooperative scheduler model: `spawn` queues producers as scheduler tasks,
    // so a synchronous recv must help drive scheduler work instead of blocking
    // forever on this Condvar. If no task can make progress and the channel is
    // still empty/open, returning a type default would silently invent a value,
    // so abort with a clear runtime panic.
    loop {
        {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            if let Some(value) = state.values.pop_front() {
                drop(state);
                wake_send_waiters(raw, channel);
                return value;
            }
            if state.closed {
                return WillowChannelValue::default();
            }
        }
        let completed = crate::scheduler::willow_sched_run();
        if completed == 0 {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            if let Some(value) = state.values.pop_front() {
                drop(state);
                wake_send_waiters(raw, channel);
                return value;
            }
            if state.closed {
                return WillowChannelValue::default();
            }
            drop(state);
            channel_abort_with("recv on empty open channel would block");
        }
    }
}

fn channel_abort_with(message: &str) -> ! {
    let ws = crate::string::willow_string_alloc(message.as_ptr(), message.len() as i64);
    crate::panic::willow_panic(ws as *const u8);
    std::process::abort();
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_send_i64(raw: *mut c_void, value: i64) {
    willow_channel_send_value(raw, WillowChannelValue { i64_value: value });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_send_bool(raw: *mut c_void, value: u8) {
    willow_channel_send_value(raw, WillowChannelValue { bool_value: value });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_send_f64(raw: *mut c_void, value: f64) {
    willow_channel_send_value(raw, WillowChannelValue { f64_value: value });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_send_ptr(raw: *mut c_void, value: *mut c_void) {
    willow_channel_send_value(raw, WillowChannelValue { ptr_value: value });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_recv_i64(raw: *mut c_void) -> i64 {
    unsafe { willow_channel_recv_value(raw).i64_value }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_recv_bool(raw: *mut c_void) -> u8 {
    unsafe { willow_channel_recv_value(raw).bool_value }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_recv_f64(raw: *mut c_void) -> f64 {
    unsafe { willow_channel_recv_value(raw).f64_value }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_recv_ptr(raw: *mut c_void) -> *mut c_void {
    unsafe { willow_channel_recv_value(raw).ptr_value }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_close(raw: *mut c_void) {
    let Some(channel) = channel_from_raw(raw) else {
        return;
    };
    let waiters: Vec<u64> = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        state.closed = true;
        channel.not_empty.notify_all();
        let mut all = state.waiters.drain_all();
        // Parked producers observe `closed` and their send becomes a no-op
        // (willow-o038). A task registered on BOTH queues (a select with a
        // recv and a send case on the same channel) is woken once: the second
        // wake is a no-op, but the extra `remove_channel_wait` would drop a
        // reverse reference this task may still need for another channel.
        let senders = state.send_waiters.drain_all();
        if !senders.is_empty() {
            let recv_ids: std::collections::HashSet<u64> = all.iter().copied().collect();
            all.extend(senders.into_iter().filter(|id| !recv_ids.contains(id)));
        }
        all
    };
    // Closing wakes every parked consumer so each can observe the closed state.
    for id in waiters {
        crate::scheduler::remove_channel_wait(id, raw as usize);
        crate::scheduler::willow_sched_wake(id);
    }
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

/// Monotonic seed for pseudo-random ready-case selection in `select`
/// (willow-0a6k.6). Selection order is pseudo-randomized to avoid SYSTEMATIC
/// source-order starvation; this is not a bounded-fairness guarantee.
#[unsafe(no_mangle)]
pub extern "C" fn willow_select_rotation() -> i64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ROTATION: AtomicU64 = AtomicU64::new(0);
    // splitmix64 finalizer over a counter: a bare counter aliases when a
    // program performs a fixed even number of selects per loop iteration
    // (k = counter % 2 would never change), the mix breaks that periodicity.
    let mut z = ROTATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) & 0x7FFF_FFFF_FFFF_FFFF) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct TestRoot(usize);

    impl GcTrace for TestRoot {
        fn trace(&self, visitor: &mut GcVisitor) {
            visitor.mark_root(self.0);
        }
    }

    #[test]
    fn channel_buffers_values_and_closes() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(10).unwrap();
        channel.send(20).unwrap();
        assert_eq!(channel.recv(), Ok(10));
        channel.close();
        assert_eq!(channel.send(30), Err(ChannelError::Closed));
        assert_eq!(channel.recv(), Ok(20));
        assert_eq!(channel.recv(), Err(ChannelError::Empty));
    }

    #[test]
    fn channel_traces_buffered_values() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(TestRoot(10)).unwrap();
        channel.send(TestRoot(20)).unwrap();

        let mut visitor = GcVisitor::default();
        channel.trace(&mut visitor);

        assert_eq!(visitor.roots(), &[10, 20]);
    }

    #[test]
    fn channel_unit_01_new_records_element_type_id() {
        let channel: RuntimeChannel<i64> = RuntimeChannel::new(42);
        assert_eq!(channel.element_type_id(), 42);
    }

    #[test]
    fn channel_unit_02_new_starts_empty() {
        let channel: RuntimeChannel<i64> = RuntimeChannel::new(1);
        assert_eq!(channel.len(), 0);
    }

    #[test]
    fn channel_unit_03_new_starts_open() {
        let channel: RuntimeChannel<i64> = RuntimeChannel::new(1);
        assert!(!channel.is_closed());
    }

    #[test]
    fn channel_unit_04_recv_empty_returns_empty() {
        let mut channel: RuntimeChannel<i64> = RuntimeChannel::new(1);
        assert_eq!(channel.recv(), Err(ChannelError::Empty));
    }

    #[test]
    fn channel_unit_05_send_increments_len() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(10).unwrap();
        assert_eq!(channel.len(), 1);
    }

    #[test]
    fn channel_unit_06_recv_decrements_len() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(10).unwrap();
        channel.send(20).unwrap();
        assert_eq!(channel.recv(), Ok(10));
        assert_eq!(channel.len(), 1);
    }

    #[test]
    fn channel_unit_07_preserves_fifo_order_for_three_values() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(1).unwrap();
        channel.send(2).unwrap();
        channel.send(3).unwrap();
        assert_eq!(channel.recv(), Ok(1));
        assert_eq!(channel.recv(), Ok(2));
        assert_eq!(channel.recv(), Ok(3));
    }

    #[test]
    fn channel_unit_08_close_is_idempotent() {
        let mut channel: RuntimeChannel<i64> = RuntimeChannel::new(1);
        channel.close();
        channel.close();
        assert!(channel.is_closed());
    }

    #[test]
    fn channel_unit_09_send_after_close_does_not_enqueue() {
        let mut channel = RuntimeChannel::new(1);
        channel.close();
        assert_eq!(channel.send(10), Err(ChannelError::Closed));
        assert_eq!(channel.len(), 0);
    }

    #[test]
    fn channel_unit_10_recv_after_close_drains_existing_value() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(10).unwrap();
        channel.close();
        assert_eq!(channel.recv(), Ok(10));
        assert_eq!(channel.recv(), Err(ChannelError::Empty));
    }

    #[test]
    fn channel_unit_11_abi_i64_send_recv_fifo() {
        let ch = willow_channel_new(0);
        willow_channel_send_i64(ch, 10);
        willow_channel_send_i64(ch, 20);
        assert_eq!(willow_channel_recv_i64(ch), 10);
        assert_eq!(willow_channel_recv_i64(ch), 20);
    }

    #[test]
    fn channel_unit_12_abi_bool_send_recv() {
        let ch = willow_channel_new(0);
        willow_channel_send_bool(ch, 1);
        assert_eq!(willow_channel_recv_bool(ch), 1);
    }

    #[test]
    fn channel_unit_13_abi_f64_send_recv() {
        let ch = willow_channel_new(0);
        willow_channel_send_f64(ch, 2.5);
        assert_eq!(willow_channel_recv_f64(ch), 2.5);
    }

    #[test]
    fn channel_unit_14_abi_recv_closed_empty_returns_zero() {
        let ch = willow_channel_new(0);
        willow_channel_close(ch);
        assert_eq!(willow_channel_recv_i64(ch), 0);
    }

    // willow-vynv.1: send wakes EVERY parked waiter (a cancelled head waiter
    // must not swallow the single wake and starve live consumers).
    #[test]
    fn send_drains_all_waiters() {
        let raw = willow_channel_new(0);
        let channel = channel_from_raw(raw).unwrap();
        {
            let mut state = channel.state.lock().unwrap();
            state.waiters.register(901);
            state.waiters.register(902);
        }
        willow_channel_send_value(raw, WillowChannelValue { i64_value: 1 });
        let state = channel.state.lock().unwrap();
        assert!(
            state.waiters.is_empty(),
            "all waiters must be drained/woken on send, not just the head"
        );
    }

    // willow-p4er: channels are GC-managed — unreachable ones are reclaimed,
    // rooted ones survive collection with their queued values intact.
    #[test]
    fn unreachable_channels_are_reclaimed() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let before = crate::gc::willow_gc_allocated_bytes();
        for _ in 0..1000 {
            let ch = willow_channel_new(0);
            assert!(!ch.is_null());
        }
        assert!(crate::gc::willow_gc_allocated_bytes() > before);
        crate::gc::willow_gc_collect();
        assert_eq!(
            crate::gc::willow_gc_allocated_bytes(),
            before,
            "unreferenced channels must be swept"
        );
    }

    #[test]
    fn gc_sweep_drops_channel_owned_queue_buffers() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let before = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst);
        const CHANNELS: usize = 256;
        for _ in 0..CHANNELS {
            let raw = willow_channel_new(0);
            let channel = channel_from_raw(raw).unwrap();
            let mut state = channel.state.lock().unwrap();
            for value in 0..64 {
                state
                    .values
                    .push_back(WillowChannelValue { i64_value: value });
                state.waiters.register(value as u64 + 1);
            }
        }

        crate::gc::willow_gc_collect();

        let dropped = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst) - before;
        assert!(
            dropped >= CHANNELS,
            "GC sweep must run WillowAbiChannel::drop for every unreachable channel; dropped {dropped}"
        );
    }

    #[test]
    fn channel_gc_hooks_register_once_per_registry_generation() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        ensure_channel_registered();
        let generation = crate::gc::registry_generation();
        let registrations = CHANNEL_REGISTRATION_COUNT.load(std::sync::atomic::Ordering::SeqCst);

        for _ in 0..10_000 {
            ensure_channel_registered();
        }
        assert_eq!(
            CHANNEL_REGISTRATION_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            registrations,
            "same-generation channel creation must stay on the atomic fast path"
        );

        crate::gc::reset_internal_for_test();
        assert_ne!(crate::gc::registry_generation(), generation);
        ensure_channel_registered();
        assert_eq!(
            CHANNEL_REGISTRATION_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            registrations + 1,
            "the first channel after a GC reset must reinstall both hooks once"
        );
    }

    #[test]
    fn rooted_channel_survives_collection_with_values() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let mut slot = willow_channel_new(0) as *mut u8;
        crate::gc::willow_push_root(&mut slot as *mut *mut u8);
        willow_channel_send_value(slot as *mut c_void, WillowChannelValue { i64_value: 42 });
        crate::gc::willow_gc_collect();
        let channel = channel_from_raw(slot as *mut c_void).unwrap();
        let got = channel
            .state
            .lock()
            .unwrap()
            .values
            .pop_front()
            .map(|v| unsafe { v.i64_value });
        crate::gc::willow_pop_roots(1);
        assert_eq!(got, Some(42), "rooted channel + queued value must survive");
    }

    #[test]
    fn cancelled_task_is_purged_from_all_waiter_queues() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        // Purge now walks the task-side REVERSE references (willow-p4er), so
        // the fixture must register the way recv_ready does: waiter queue
        // entry + record_channel_wait on the task. Task 7 must exist.
        let (t7, t9) = crate::scheduler::with_global_for_test(|sched| {
            (sched.spawn_placeholder(), sched.spawn_placeholder())
        });
        let first = willow_channel_new(0);
        let second = willow_channel_new(0);
        for raw in [first, second] {
            let channel = channel_from_raw(raw).unwrap();
            let mut state = channel.state.lock().unwrap();
            // The duplicate registration must collapse: `register` is the only
            // way in, and it rejects an id already in the queue.
            for id in [t7, t9, t7] {
                state.waiters.register(id);
            }
            drop(state);
            crate::scheduler::record_channel_wait(t7, raw as usize);
        }

        purge_task(t7);

        for raw in [first, second] {
            let channel = channel_from_raw(raw).unwrap();
            assert_eq!(channel.state.lock().unwrap().waiters.live(), vec![t9]);
        }
    }

    #[test]
    fn normal_waiter_removal_clears_task_reverse_references() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (unregister_task, send_task, close_task) =
            crate::scheduler::with_global_for_test(|sched| {
                (
                    sched.spawn_placeholder(),
                    sched.spawn_placeholder(),
                    sched.spawn_placeholder(),
                )
            });

        let unregister_channel = willow_channel_new(0);
        channel_from_raw(unregister_channel)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .waiters
            .register(unregister_task);
        crate::scheduler::record_channel_wait(unregister_task, unregister_channel as usize);
        crate::scheduler::with_global_for_test(|sched| sched.set_running(unregister_task));
        willow_channel_unregister_waiter(unregister_channel);
        crate::scheduler::with_global_for_test(|sched| sched.clear_running());
        assert!(
            crate::scheduler::take_channel_waits(unregister_task).is_empty(),
            "select unregister must remove the task-side channel address"
        );

        let send_channel = willow_channel_new(0);
        channel_from_raw(send_channel)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .waiters
            .register(send_task);
        crate::scheduler::record_channel_wait(send_task, send_channel as usize);
        willow_channel_send_i64(send_channel, 1);
        assert!(
            crate::scheduler::take_channel_waits(send_task).is_empty(),
            "send wake must remove the task-side channel address"
        );

        let close_channel = willow_channel_new(0);
        channel_from_raw(close_channel)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .waiters
            .register(close_task);
        crate::scheduler::record_channel_wait(close_task, close_channel as usize);
        willow_channel_close(close_channel);
        assert!(
            crate::scheduler::take_channel_waits(close_task).is_empty(),
            "close wake must remove the task-side channel address"
        );

        crate::gc::willow_gc_collect();
    }

    // ── Bounded channels (willow-o038) ───────────────────────────────────────

    fn capacity_of(raw: *mut c_void) -> Option<usize> {
        channel_from_raw(raw)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .capacity
    }

    fn queued(raw: *mut c_void) -> usize {
        channel_from_raw(raw)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .values
            .len()
    }

    fn send_waiter_ids(raw: *mut c_void) -> Vec<u64> {
        channel_from_raw(raw)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .live()
    }

    #[test]
    fn bounded_unit_01_new_is_unbounded_and_with_capacity_is_bounded() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        assert_eq!(capacity_of(willow_channel_new(0)), None);
        assert_eq!(capacity_of(willow_channel_new_bounded(0, 3)), Some(3));
    }

    #[test]
    fn bounded_unit_02_try_send_fills_then_reports_full() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let ch = willow_channel_new_bounded(0, 2);
        assert_eq!(willow_channel_try_send_i64(ch, 1), 1);
        assert_eq!(willow_channel_try_send_i64(ch, 2), 1);
        assert_eq!(willow_channel_try_send_i64(ch, 3), 0);
        assert_eq!(queued(ch), 2);
    }

    #[test]
    fn bounded_unit_03_recv_frees_a_slot_for_the_next_send() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let ch = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(ch, 1), 1);
        assert_eq!(willow_channel_try_send_i64(ch, 2), 0);
        assert_eq!(willow_channel_recv_i64(ch), 1);
        assert_eq!(willow_channel_try_send_i64(ch, 2), 1);
        assert_eq!(willow_channel_recv_i64(ch), 2);
    }

    #[test]
    fn bounded_unit_04_send_ready_tracks_fullness() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let ch = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_send_ready(ch), 1);
        willow_channel_try_send_i64(ch, 1);
        assert_eq!(willow_channel_send_ready(ch), 0);
        willow_channel_recv_i64(ch);
        assert_eq!(willow_channel_send_ready(ch), 1);
    }

    #[test]
    fn bounded_unit_05_unbounded_send_ready_is_always_one() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let ch = willow_channel_new(0);
        for value in 0..64 {
            assert_eq!(willow_channel_send_ready(ch), 1);
            assert_eq!(willow_channel_try_send_i64(ch, value), 1);
        }
        assert_eq!(willow_channel_send_ready(ch), 1);
    }

    #[test]
    fn bounded_unit_06_closed_full_channel_accepts_sends_as_noops() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let ch = willow_channel_new_bounded(0, 1);
        willow_channel_try_send_i64(ch, 1);
        willow_channel_close(ch);
        // Send-on-closed is a documented no-op, so it must never report FULL:
        // that would park a producer nobody is going to wake.
        assert_eq!(willow_channel_send_ready(ch), 1);
        assert_eq!(willow_channel_try_send_i64(ch, 2), 1);
        assert_eq!(queued(ch), 1);
    }

    #[test]
    fn bounded_unit_07_close_drains_send_waiters() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_placeholder());
        let ch = willow_channel_new_bounded(0, 1);
        willow_channel_try_send_i64(ch, 1);
        channel_from_raw(ch)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .register(task);
        crate::scheduler::record_channel_wait(task, ch as usize);
        willow_channel_close(ch);
        assert!(
            send_waiter_ids(ch).is_empty(),
            "close must wake every parked producer"
        );
        assert!(crate::scheduler::take_channel_waits(task).is_empty());
    }

    #[test]
    fn bounded_unit_08_recv_wakes_exactly_one_send_waiter() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (first, second) = crate::scheduler::with_global_for_test(|sched| {
            (
                sched.spawn_parked_placeholder(),
                sched.spawn_parked_placeholder(),
            )
        });
        let ch = willow_channel_new_bounded(0, 1);
        willow_channel_try_send_i64(ch, 1);
        {
            let mut state = channel_from_raw(ch).unwrap().state.lock().unwrap();
            state.send_waiters.register(first);
            state.send_waiters.register(second);
        }
        crate::scheduler::record_channel_wait(first, ch as usize);
        crate::scheduler::record_channel_wait(second, ch as usize);

        assert_eq!(willow_channel_recv_i64(ch), 1);
        assert_eq!(
            send_waiter_ids(ch),
            vec![second],
            "one free slot must wake only the oldest live producer"
        );
        crate::scheduler::with_global_for_test(|sched| {
            assert_eq!(
                sched.task_state(first),
                Some(crate::task::RuntimeTaskState::Ready)
            );
            assert_eq!(
                sched.task_state(second),
                Some(crate::task::RuntimeTaskState::Parked)
            );
        });
        assert_eq!(
            crate::scheduler::take_channel_waits(first),
            vec![ch as usize],
            "the woken producer keeps a reverse reference until it sends or defects"
        );
        assert_eq!(
            crate::scheduler::take_channel_waits(second),
            vec![ch as usize],
            "producers left parked must retain their reverse reference"
        );
    }

    #[test]
    fn bounded_unit_09_select_defection_compensates_the_wake_one_handoff() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (first, second) = crate::scheduler::with_global_for_test(|sched| {
            (
                sched.spawn_parked_placeholder(),
                sched.spawn_parked_placeholder(),
            )
        });
        let ch = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(ch, 1), 1);
        for task in [first, second] {
            crate::scheduler::with_current_task_for_test(task, || {
                assert_eq!(willow_channel_send_ready(ch), 0);
            });
        }

        assert_eq!(willow_channel_recv_i64(ch), 1);
        crate::scheduler::with_global_for_test(|sched| {
            assert_eq!(
                sched.task_state(first),
                Some(crate::task::RuntimeTaskState::Ready)
            );
            assert_eq!(
                sched.task_state(second),
                Some(crate::task::RuntimeTaskState::Parked)
            );
        });

        // The first select re-probes, but another arm wins. Its unregister
        // must pass the still-empty slot to the second producer.
        crate::scheduler::with_current_task_for_test(first, || {
            willow_channel_unregister_waiter(ch);
        });
        crate::scheduler::with_global_for_test(|sched| {
            assert_eq!(
                sched.task_state(second),
                Some(crate::task::RuntimeTaskState::Ready)
            );
        });
        assert!(send_waiter_ids(ch).is_empty());
        assert_eq!(
            crate::scheduler::take_channel_waits(second),
            vec![ch as usize],
            "the replacement handoff remains cancellable until consumed"
        );
    }

    #[test]
    fn bounded_unit_10_cancelled_handoff_wakes_the_next_producer() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (first, second) = crate::scheduler::with_global_for_test(|sched| {
            (
                sched.spawn_parked_placeholder(),
                sched.spawn_parked_placeholder(),
            )
        });
        let ch = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(ch, 1), 1);
        for task in [first, second] {
            crate::scheduler::with_current_task_for_test(task, || {
                assert_eq!(willow_channel_send_ready(ch), 0);
            });
        }

        assert_eq!(willow_channel_recv_i64(ch), 1);
        crate::scheduler::willow_sched_cancel(first);
        assert_eq!(
            crate::scheduler::willow_sched_run_until(first),
            0,
            "cancellation is terminal but not a completed result"
        );
        crate::scheduler::with_global_for_test(|sched| {
            assert_eq!(sched.task_state(first), None);
            assert_eq!(
                sched.task_state(second),
                Some(crate::task::RuntimeTaskState::Ready),
                "terminal purge must compensate a cancelled send handoff"
            );
        });
        assert!(send_waiter_ids(ch).is_empty());
        assert_eq!(
            crate::scheduler::take_channel_waits(second),
            vec![ch as usize]
        );
    }

    #[test]
    fn bounded_unit_11_successful_retry_consumes_the_handoff_reference() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_parked_placeholder());
        let ch = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(ch, 1), 1);
        crate::scheduler::with_current_task_for_test(task, || {
            assert_eq!(willow_channel_send_ready(ch), 0);
        });

        assert_eq!(willow_channel_recv_i64(ch), 1);
        crate::scheduler::with_current_task_for_test(task, || {
            assert_eq!(willow_channel_try_send_i64(ch, 2), 1);
        });
        assert!(
            crate::scheduler::take_channel_waits(task).is_empty(),
            "a successful retry consumed the send handoff"
        );
        assert_eq!(willow_channel_recv_i64(ch), 2);
    }

    #[test]
    fn bounded_unit_12_purge_clears_send_waiters_too() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_placeholder());
        let ch = willow_channel_new_bounded(0, 1);
        willow_channel_try_send_i64(ch, 1);
        channel_from_raw(ch)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .register(task);
        crate::scheduler::record_channel_wait(task, ch as usize);
        purge_task(task);
        assert!(
            send_waiter_ids(ch).is_empty(),
            "cancelling a parked producer must purge its send registration"
        );
    }

    #[test]
    fn bounded_unit_13_unregister_waiter_clears_send_side() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_placeholder());
        let ch = willow_channel_new_bounded(0, 1);
        willow_channel_try_send_i64(ch, 1);
        channel_from_raw(ch)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .register(task);
        crate::scheduler::record_channel_wait(task, ch as usize);
        crate::scheduler::with_current_task_for_test(task, || {
            willow_channel_unregister_waiter(ch);
        });
        assert!(
            send_waiter_ids(ch).is_empty(),
            "a select that picked another case must unregister its send waiter"
        );
    }

    #[test]
    fn bounded_unit_14_ptr_elements_are_traced_while_buffer_is_full() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let ch = willow_channel_new_bounded(1, 1);
        let text = "queued";
        let value = crate::string::willow_string_alloc(text.as_ptr(), text.len() as i64);
        assert_eq!(willow_channel_try_send_ptr(ch, value as *mut c_void), 1);
        assert_eq!(willow_channel_try_send_ptr(ch, value as *mut c_void), 0);
        let mut slots: Vec<*mut *mut u8> = Vec::new();
        unsafe { trace_channel(ch as *mut u8, &mut slots) };
        assert_eq!(slots.len(), 1, "the queued pointer must be a traced slot");
    }

    #[test]
    fn bounded_unit_15_bool_and_f64_elements_respect_capacity() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let flags = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_bool(flags, 1), 1);
        assert_eq!(willow_channel_try_send_bool(flags, 0), 0);
        assert_eq!(willow_channel_recv_bool(flags), 1);

        let reals = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_f64(reals, 1.5), 1);
        assert_eq!(willow_channel_try_send_f64(reals, 2.5), 0);
        assert_eq!(willow_channel_recv_f64(reals), 1.5);
    }

    #[test]
    fn bounded_unit_16_stale_head_does_not_swallow_the_single_wake() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (stale, live) = crate::scheduler::with_global_for_test(|sched| {
            let stale = sched.spawn_parked_placeholder();
            let live = sched.spawn_parked_placeholder();
            sched.complete(stale);
            (stale, live)
        });
        let ch = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(ch, 1), 1);
        {
            let mut state = channel_from_raw(ch).unwrap().state.lock().unwrap();
            state.send_waiters.register(stale);
            state.send_waiters.register(live);
        }
        crate::scheduler::record_channel_wait(stale, ch as usize);
        crate::scheduler::record_channel_wait(live, ch as usize);

        assert_eq!(willow_channel_recv_i64(ch), 1);
        assert!(
            send_waiter_ids(ch).is_empty(),
            "the stale head and the one producer actually woken are both consumed"
        );
        crate::scheduler::with_global_for_test(|sched| {
            assert_eq!(
                sched.task_state(live),
                Some(crate::task::RuntimeTaskState::Ready)
            );
        });
        assert!(crate::scheduler::take_channel_waits(stale).is_empty());
        assert_eq!(
            crate::scheduler::take_channel_waits(live),
            vec![ch as usize],
            "the live producer owns the handoff until send/unregister/cancel"
        );
    }

    // ── O(1) waiter membership (willow-ezs.1.2) ──────────────────────────────
    //
    // Registration used `VecDeque::contains`, so parking 10,000 tasks on ONE
    // channel cost O(n^2) and every select loser's unregister was another O(n)
    // scan. `WaiterQueue` keeps a membership set beside the FIFO order.
    // Perspectives 1-15 of willow-ezs.1.2 (16-28 cover the scheduler's
    // blocked-syscall counter, in `scheduler.rs`):
    //
    //  1. a first registration is accepted, a duplicate is rejected
    //  2. 10k distinct registrations on one channel are all live, in order
    //  3. re-registering all 10k is rejected and does not grow the queue
    //  4. a removed waiter is not woken by a later drain
    //  5. removing an unregistered id is a no-op
    //  6. re-registering after a remove works and wakes exactly once
    //  7. drain_all reports live waiters in registration order
    //  8. drain_all skips tombstones and empties both order and membership
    //  9. churn cannot grow the backing queue without bound (compaction)
    // 10. compaction preserves the live set and its order
    // 11. recv_ready registers a task once and records one reverse wait
    // 12. send_ready registers a producer once on a FULL bounded channel
    // 13. purge_task clears a task from BOTH queues of every channel
    // 14. unregister_waiter clears both queues and the reverse reference
    // 15. close wakes a task registered on both queues exactly once

    #[test]
    fn wq_01_duplicate_registration_is_rejected() {
        let mut queue = WaiterQueue::default();
        assert!(queue.register(1));
        assert!(!queue.register(1), "duplicates must not be queued twice");
        assert!(queue.contains(&1));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.live(), vec![1]);
    }

    #[test]
    fn wq_02_ten_thousand_distinct_waiters_stay_live_and_ordered() {
        let mut queue = WaiterQueue::default();
        for id in 0..10_000u64 {
            assert!(queue.register(id));
        }
        assert_eq!(queue.len(), 10_000);
        assert_eq!(queue.live(), (0..10_000u64).collect::<Vec<_>>());
    }

    #[test]
    fn wq_03_reregistering_ten_thousand_waiters_does_not_grow_the_queue() {
        let mut queue = WaiterQueue::default();
        for id in 0..10_000u64 {
            queue.register(id);
        }
        let order_len = queue.queued_entries();
        for id in 0..10_000u64 {
            assert!(!queue.register(id));
        }
        assert_eq!(queue.queued_entries(), order_len);
        assert_eq!(queue.len(), 10_000);
    }

    #[test]
    fn wq_04_removed_waiter_is_not_woken() {
        let mut queue = WaiterQueue::default();
        queue.register(1);
        queue.register(2);
        queue.register(3);
        queue.remove(2);
        assert!(!queue.contains(&2));
        assert_eq!(queue.drain_all(), vec![1, 3]);
    }

    #[test]
    fn wq_05_removing_an_unregistered_id_is_a_noop() {
        let mut queue = WaiterQueue::default();
        queue.register(1);
        queue.remove(99);
        queue.remove(99);
        assert_eq!(queue.live(), vec![1]);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn wq_06_reregistration_after_removal_wakes_exactly_once() {
        let mut queue = WaiterQueue::default();
        queue.register(1);
        queue.register(2);
        queue.remove(1);
        assert!(queue.register(1), "a removed waiter can park again");
        let woken = queue.drain_all();
        assert_eq!(
            woken,
            vec![2, 1],
            "re-registration must move the task behind existing live waiters"
        );
    }

    #[test]
    fn wq_07_drain_reports_registration_order() {
        let mut queue = WaiterQueue::default();
        for id in [5u64, 4, 9, 1] {
            queue.register(id);
        }
        assert_eq!(queue.drain_all(), vec![5, 4, 9, 1]);
    }

    #[test]
    fn wq_08_drain_empties_order_and_membership() {
        let mut queue = WaiterQueue::default();
        for id in 0..32u64 {
            queue.register(id);
        }
        for id in (0..32u64).step_by(2) {
            queue.remove(id);
        }
        let woken = queue.drain_all();
        assert_eq!(
            woken,
            (0..32u64).filter(|id| id % 2 == 1).collect::<Vec<_>>()
        );
        assert!(queue.is_empty());
        assert_eq!(queue.queued_entries(), 0);
        assert!(queue.is_empty());
        assert!(queue.drain_all().is_empty());
    }

    #[test]
    fn wq_09_churn_cannot_grow_the_backing_queue_without_bound() {
        let mut queue = WaiterQueue::default();
        // A select loop: one task parks and unparks over and over. Tombstones
        // must be reclaimed, or `order` would reach 100_000 entries.
        for id in 0..100_000u64 {
            queue.register(id);
            queue.remove(id);
        }
        assert!(queue.is_empty());
        assert!(
            queue.queued_entries() <= 64,
            "tombstones must be compacted away; order = {}",
            queue.queued_entries()
        );
    }

    #[test]
    fn wq_10_compaction_preserves_live_waiters_and_order() {
        let mut queue = WaiterQueue::default();
        for id in 0..1_000u64 {
            queue.register(id);
        }
        // Remove nine of every ten, forcing repeated compaction.
        for id in 0..1_000u64 {
            if id % 10 != 0 {
                queue.remove(id);
            }
        }
        let expected: Vec<u64> = (0..1_000u64).filter(|id| id % 10 == 0).collect();
        assert_eq!(queue.live(), expected);
        assert_eq!(queue.drain_all(), expected);
    }

    #[test]
    fn wq_11_recv_ready_registers_each_task_once() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_placeholder());
        crate::scheduler::with_global_for_test(|sched| sched.set_running(task));

        let raw = willow_channel_new(0);
        for _ in 0..1_000 {
            assert_eq!(willow_channel_recv_ready(raw), 0);
        }
        let state = channel_from_raw(raw).unwrap().state.lock().unwrap();
        assert_eq!(state.waiters.live(), vec![task]);
        assert_eq!(state.waiters.queued_entries(), 1);
        drop(state);

        crate::scheduler::with_global_for_test(|sched| sched.clear_running());
        assert_eq!(
            crate::scheduler::take_channel_waits(task),
            vec![raw as usize],
            "a repeated probe must not duplicate the reverse reference"
        );
    }

    #[test]
    fn wq_12_send_ready_registers_each_producer_once() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_placeholder());
        crate::scheduler::with_global_for_test(|sched| sched.set_running(task));

        let raw = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(raw, 1), 1);
        for _ in 0..1_000 {
            assert_eq!(willow_channel_send_ready(raw), 0);
            assert_eq!(willow_channel_try_send_i64(raw, 2), 0);
        }
        assert_eq!(send_waiter_ids(raw), vec![task]);
        let state = channel_from_raw(raw).unwrap().state.lock().unwrap();
        assert_eq!(state.send_waiters.queued_entries(), 1);
        drop(state);

        crate::scheduler::with_global_for_test(|sched| sched.clear_running());
        assert_eq!(
            crate::scheduler::take_channel_waits(task),
            vec![raw as usize]
        );
    }

    #[test]
    fn wq_13_purge_task_clears_both_queues() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (victim, other) = crate::scheduler::with_global_for_test(|sched| {
            (sched.spawn_placeholder(), sched.spawn_placeholder())
        });
        let raw = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(raw, 1), 1);
        {
            let mut state = channel_from_raw(raw).unwrap().state.lock().unwrap();
            state.waiters.register(victim);
            state.waiters.register(other);
            state.send_waiters.register(victim);
            state.send_waiters.register(other);
        }
        crate::scheduler::record_channel_wait(victim, raw as usize);

        purge_task(victim);

        let state = channel_from_raw(raw).unwrap().state.lock().unwrap();
        assert_eq!(state.waiters.live(), vec![other]);
        assert_eq!(state.send_waiters.live(), vec![other]);
        assert!(!state.waiters.contains(&victim));
        assert!(!state.send_waiters.contains(&victim));
    }

    #[test]
    fn wq_14_unregister_waiter_clears_both_queues_and_reverse_reference() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|sched| sched.spawn_placeholder());
        let raw = willow_channel_new_bounded(0, 1);
        {
            let mut state = channel_from_raw(raw).unwrap().state.lock().unwrap();
            state.waiters.register(task);
            state.send_waiters.register(task);
        }
        crate::scheduler::record_channel_wait(task, raw as usize);

        crate::scheduler::with_global_for_test(|sched| sched.set_running(task));
        willow_channel_unregister_waiter(raw);
        crate::scheduler::with_global_for_test(|sched| sched.clear_running());

        let state = channel_from_raw(raw).unwrap().state.lock().unwrap();
        assert!(state.waiters.is_empty());
        assert!(state.send_waiters.is_empty());
        drop(state);
        assert!(crate::scheduler::take_channel_waits(task).is_empty());
    }

    #[test]
    fn wq_15_close_wakes_a_dual_registered_task_once() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        // A select with a recv case AND a send case on the same channel parks
        // the task on both queues; close must not wake it twice (the second
        // `remove_channel_wait` would drop a reference it still needs).
        let task = crate::scheduler::with_global_for_test(|sched| {
            let id = sched.spawn_placeholder();
            sched.park(id);
            id
        });
        let raw = willow_channel_new_bounded(0, 1);
        let other = willow_channel_new(0);
        {
            let mut state = channel_from_raw(raw).unwrap().state.lock().unwrap();
            state.waiters.register(task);
            state.send_waiters.register(task);
        }
        crate::scheduler::record_channel_wait(task, raw as usize);
        crate::scheduler::record_channel_wait(task, other as usize);

        willow_channel_close(raw);

        assert_eq!(
            crate::scheduler::take_channel_waits(task),
            vec![other as usize],
            "close must drop only the closed channel's reverse reference"
        );
    }
}
