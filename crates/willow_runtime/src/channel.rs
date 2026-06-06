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

/// Registry of GC-element channels so the collector can trace their buffered
/// values. Channels are program-lifetime (leaked `Box::into_raw`), so entries
/// are never removed (willow-dsw).
static CHANNEL_GC_REGISTRY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_new(is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(Box::new(WillowAbiChannel::new(is_ref)));
    if is_ref {
        CHANNEL_GC_REGISTRY
            .lock()
            .expect("channel registry poisoned")
            .push(raw as usize);
    }
    raw as *mut c_void
}

/// Live GC roots held in channel buffers: for every registered GC-element
/// channel, each queued pointer value is a root. Called by the collector after
/// the runtime-root snapshot (willow-dsw).
pub(crate) fn channel_gc_roots() -> Vec<*mut u8> {
    let registry = CHANNEL_GC_REGISTRY
        .lock()
        .expect("channel registry poisoned");
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
    let waiter = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if state.closed {
            return;
        }
        state.values.push_back(value);
        channel.not_empty.notify_one();
        // Hand the value to one parked cooperative consumer (FIFO).
        state.waiters.pop_front()
    };
    // Wake outside the channel lock (willow_sched_wake takes the scheduler lock).
    if let Some(id) = waiter {
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
    if current != 0 {
        crate::gc::stress_collect("scheduler");
    }
    0
}

fn willow_channel_recv_value(raw: *mut c_void) -> WillowChannelValue {
    let Some(channel) = channel_from_raw(raw) else {
        return WillowChannelValue::default();
    };
    // Cooperative single-threaded model: `spawn` runs producers as scheduler
    // tasks on THIS thread, not OS threads, so blocking on a cross-thread Condvar
    // would deadlock. Instead, when the channel is empty we drive ready scheduler
    // tasks (producers) and retry. If no task can make progress and the channel
    // is still empty/open, returning a type default would silently invent a
    // value, so abort with a clear runtime panic.
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
}

impl<T: GcTrace> GcTrace for RuntimeChannel<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        for value in &self.buffer {
            value.trace(visitor);
        }
    }
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
}
