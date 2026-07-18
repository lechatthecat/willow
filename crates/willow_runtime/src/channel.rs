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

/// Registry of all channels. Channels are program-lifetime (leaked
/// `Box::into_raw`), so entries are never removed. Keeping scalar channels here
/// too lets cancellation purge select/recv waiter ids instead of retaining
/// stale registrations until the channel is next used.
static CHANNEL_REGISTRY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_new(is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(Box::new(WillowAbiChannel::new(is_ref)));
    CHANNEL_REGISTRY
        .lock()
        .expect("channel registry poisoned")
        .push(raw as usize);
    raw as *mut c_void
}

/// Live GC roots held in channel buffers: for every registered GC-element
/// channel, each queued pointer value is a root. Called by the collector after
/// the runtime-root snapshot (willow-dsw).
pub(crate) fn channel_gc_roots() -> Vec<*mut u8> {
    let registry = CHANNEL_REGISTRY.lock().expect("channel registry poisoned");
    let mut roots = Vec::new();
    for &addr in registry.iter() {
        let channel = unsafe { &*(addr as *const WillowAbiChannel) };
        if !channel.is_ref {
            continue;
        }
        if let Ok(state) = channel.state.lock() {
            for value in &state.values {
                let ptr = unsafe { value.ptr_value } as *mut u8;
                if !ptr.is_null() {
                    roots.push(ptr);
                }
            }
        }
    }
    roots
}

/// Remove a completed/cancelled task from every channel waiter queue. This is
/// needed for a task cancelled while parked on `select`: no case is chosen, so
/// generated unregister-all code never runs.
pub(crate) fn purge_task(task_id: u64) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let registry = CHANNEL_REGISTRY.lock().expect("channel registry poisoned");
    for &address in registry.iter() {
        let channel = unsafe { &*(address as *const WillowAbiChannel) };
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        state.waiters.retain(|&waiter| waiter != task_id);
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
    let Some(channel) = channel_from_raw(raw) else {
        return;
    };
    let waiters: Vec<u64> = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if state.closed {
            return;
        }
        state.values.push_back(value);
        channel.not_empty.notify_one();
        // Wake EVERY parked cooperative consumer, not just the FIFO head: a
        // CANCELLED task in the queue would silently swallow a single wake
        // (willow_sched_wake no-ops on terminal states) and live consumers
        // would sleep forever (willow-vynv.1). Woken losers that find the
        // buffer empty simply re-register.
        state.waiters.drain(..).collect()
    };
    // Wake outside the channel lock (willow_sched_wake takes the scheduler lock).
    for id in waiters {
        crate::scheduler::willow_sched_wake(id);
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
        state.waiters.drain(..).collect()
    };
    // Closing wakes every parked consumer so each can observe the closed state.
    for id in waiters {
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
/// (spec: fairness among simultaneously-ready cases, willow-0a6k.6). A plain
/// counter rotates the pick, which is enough to prevent starvation of a
/// lower-priority case without needing real randomness.
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

    #[test]
    fn cancelled_task_is_purged_from_all_waiter_queues() {
        let first = willow_channel_new(0);
        let second = willow_channel_new(0);
        for raw in [first, second] {
            let channel = channel_from_raw(raw).unwrap();
            channel.state.lock().unwrap().waiters.extend([7, 9, 7]);
        }

        purge_task(7);

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
                vec![9]
            );
        }
    }
}
