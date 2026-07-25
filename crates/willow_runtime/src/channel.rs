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

#[derive(Default)]
struct WillowChannelState {
    values: VecDeque<WillowChannelValue>,
    closed: bool,
    /// Cooperative consumers parked on an empty `recv`, woken FIFO by `send` /
    /// `close` (willow-dsw).
    waiters: VecDeque<u64>,
    /// Bounded capacity (`Channel<T>::with_capacity(n)`); `None` = unbounded
    /// (willow-o038).
    capacity: Option<usize>,
    /// Cooperative producers parked on a FULL bounded channel, woken
    /// drain-all by `recv` (space freed) / `close` — the same lost-wakeup
    /// discipline as recv waiters (willow-o038).
    send_waiters: VecDeque<u64>,
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
    let (sent, waiters): (bool, Vec<u64>) = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if state.closed {
            (true, Vec::new())
        } else if state_is_full(&state) {
            let current = crate::scheduler::willow_sched_current_task();
            let registered = if current != 0 && !state.send_waiters.contains(&current) {
                state.send_waiters.push_back(current);
                true
            } else {
                false
            };
            // Record the reverse reference only after releasing the channel
            // lock: the scheduler side takes its own lock, so holding both
            // would fix a second lock order.
            drop(state);
            if registered {
                crate::scheduler::record_channel_wait(current, raw as usize);
            }
            return 0;
        } else {
            if channel.is_ref {
                crate::gc::willow_gc_write_barrier(
                    raw as *mut u8,
                    unsafe { value.ptr_value } as *mut u8,
                    crate::gc::GcStoreDestination::ContainerInternal as i64,
                );
            }
            state.values.push_back(value);
            channel.not_empty.notify_one();
            (true, state.waiters.drain(..).collect())
        }
    };
    for id in waiters {
        crate::scheduler::remove_channel_wait(id, raw as usize);
        crate::scheduler::willow_sched_wake(id);
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
    if current != 0 && !state.send_waiters.contains(&current) {
        state.send_waiters.push_back(current);
        drop(state);
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
    for address in crate::scheduler::take_channel_waits(task_id) {
        let channel = unsafe { &*(address as *const WillowAbiChannel) };
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        state.waiters.retain(|&waiter| waiter != task_id);
        state.send_waiters.retain(|&waiter| waiter != task_id);
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
    if current != 0 && !state.waiters.contains(&current) {
        state.waiters.push_back(current);
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
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    state.waiters.retain(|&id| id != current);
    state.send_waiters.retain(|&id| id != current);
    drop(state);
    crate::scheduler::remove_channel_wait(current, raw as usize);
}

/// A recv freed one slot: wake ALL parked producers (drain-all, same
/// lost-wakeup discipline as recv waiters; losers re-register) (willow-o038).
fn wake_send_waiters(raw: *mut c_void, channel: &WillowAbiChannel) {
    let senders: Vec<u64> = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        state.send_waiters.drain(..).collect()
    };
    for id in senders {
        crate::scheduler::remove_channel_wait(id, raw as usize);
        crate::scheduler::willow_sched_wake(id);
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
        let mut all: Vec<u64> = state.waiters.drain(..).collect();
        // Parked producers observe `closed` and their send becomes a no-op
        // (willow-o038).
        all.extend(state.send_waiters.drain(..));
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
            state.waiters.push_back(901);
            state.waiters.push_back(902);
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
                state.waiters.push_back(value as u64 + 1);
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
            channel.state.lock().unwrap().waiters.extend([t7, t9, t7]);
            crate::scheduler::record_channel_wait(t7, raw as usize);
        }

        purge_task(t7);

        for raw in [first, second] {
            let channel = channel_from_raw(raw).unwrap();
            assert_eq!(
                channel
                    .state
                    .lock()
                    .unwrap()
                    .waiters
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![t9]
            );
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
            .push_back(unregister_task);
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
            .push_back(send_task);
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
            .push_back(close_task);
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
            .iter()
            .copied()
            .collect()
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
            .push_back(task);
        crate::scheduler::record_channel_wait(task, ch as usize);
        willow_channel_close(ch);
        assert!(
            send_waiter_ids(ch).is_empty(),
            "close must wake every parked producer"
        );
        assert!(crate::scheduler::take_channel_waits(task).is_empty());
    }

    #[test]
    fn bounded_unit_08_recv_wakes_send_waiters() {
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
            .push_back(task);
        crate::scheduler::record_channel_wait(task, ch as usize);
        assert_eq!(willow_channel_recv_i64(ch), 1);
        assert!(
            send_waiter_ids(ch).is_empty(),
            "a recv that frees a slot must wake parked producers"
        );
        assert!(crate::scheduler::take_channel_waits(task).is_empty());
    }

    #[test]
    fn bounded_unit_09_purge_clears_send_waiters_too() {
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
            .push_back(task);
        crate::scheduler::record_channel_wait(task, ch as usize);
        purge_task(task);
        assert!(
            send_waiter_ids(ch).is_empty(),
            "cancelling a parked producer must purge its send registration"
        );
    }

    #[test]
    fn bounded_unit_10_unregister_waiter_clears_send_side() {
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
            .push_back(task);
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
    fn bounded_unit_11_ptr_elements_are_traced_while_buffer_is_full() {
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
    fn bounded_unit_12_bool_and_f64_elements_respect_capacity() {
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
}
