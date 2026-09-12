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

unsafe fn snapshot_channel(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let channel = unsafe { &*(payload as *const WillowAbiChannel) };
    if channel.is_ref {
        let state = channel.state.lock().unwrap();
        children.extend(
            state
                .values
                .iter()
                .map(|value| unsafe { value.ptr_value as *mut u8 }),
        );
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
            .with_concurrent_trace(snapshot_channel),
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
        channel_raise_with(
            "channel capacity must be positive (rendezvous channels are not supported)",
        );
        return std::ptr::null_mut();
    }
    let raw = willow_channel_new(is_ref);
    if let Some(channel) = unsafe { channel_from_raw(raw) } {
        channel
            .state
            .lock()
            .expect("channel mutex poisoned")
            .capacity = Some(capacity as usize);
    }
    raw
}

fn state_is_full(state: &WillowChannelState) -> bool {
    matches!(state.capacity, Some(cap) if state.values.len() + state.send_handoffs.len() >= cap)
}

fn token(raw: *mut c_void, role: ChannelRole, generation: u64) -> ChannelOwnershipToken {
    ChannelOwnershipToken {
        channel: raw as usize,
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
                unsafe { value.ptr_value } as *mut u8,
                crate::gc::GcStoreDestination::ContainerInternal as i64,
            );
        }
        state.values.push_back(value);
        channel.not_empty.notify_one();
    }
    wake_recv_waiters(raw, channel);
    wake_send_waiters(raw, channel);
    1
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
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 1;
    };
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    let current = crate::scheduler::willow_sched_current_task();
    if state.closed || state.send_handoffs.contains_key(&current) || !state_is_full(&state) {
        return 1;
    }
    let registered = register_wait(raw, &mut state.send_waiters, current, ChannelRole::SendWait);
    drop(state);
    drop(_no_preempt);
    if registered {
        crate::observability::record(
            crate::observability::RuntimeEventKind::ChannelWait,
            None,
            current,
            1,
        );
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
        let raw = address as *mut c_void;
        let channel = unsafe { &*(raw as *const WillowAbiChannel) };
        {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            for owner in owners {
                remove_exact(&mut state, task_id, owner);
            }
        }
        wake_recv_waiters(raw, channel);
        wake_send_waiters(raw, channel);
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

/// Cooperative `recv` readiness probe (willow-dsw): returns 1 if a value is
/// available OR the channel is closed (the caller then reads the value or
/// observes the closed-empty language panic via `willow_channel_recv_*`);
/// returns 0 if the channel is empty and open,
/// after registering the currently-running task as a waiter — the caller's poll
/// fn then returns Pending and is woken by a later `send`/`close`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_recv_ready(raw: *mut c_void) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 1;
    };
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    let current = crate::scheduler::willow_sched_current_task();
    if state.recv_claims.contains_key(&current) {
        return 1;
    }
    if state.values.len() > state.recv_claims.len() {
        if current == 0 {
            return 1;
        }
        let generation = next_generation(&mut state);
        let claim = token(raw, ChannelRole::RecvClaim, generation);
        let installed = if let Some(wait) = state.waiters.ticket(current) {
            let installed = crate::scheduler::transition_channel_ownership(
                current,
                token(raw, ChannelRole::RecvWait, wait),
                claim,
            );
            state.waiters.remove_ticket(current, wait);
            if !installed {
                crate::scheduler::clear_channel_ownership(
                    current,
                    token(raw, ChannelRole::RecvWait, wait),
                );
            }
            installed
        } else {
            crate::scheduler::install_channel_ownership(current, claim)
        };
        if installed {
            state.recv_claims.insert(current, generation);
            return 1;
        }
        return 0;
    }
    // A closed channel with reserved values is not empty for its owners.
    if state.closed && state.values.is_empty() {
        return 1;
    }
    let registered = register_wait(raw, &mut state.waiters, current, ChannelRole::RecvWait);
    drop(state);
    drop(_no_preempt);
    if registered {
        crate::observability::record(
            crate::observability::RuntimeEventKind::ChannelWait,
            None,
            current,
            0,
        );
    }
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
    willow_channel_select_cleanup(raw, std::ptr::null_mut(), -1);
}

/// Called once per distinct runtime channel by select, before committing its
/// winning operation. Direction: 0 recv, 1 send, -1 non-channel winner.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_select_cleanup(
    raw: *mut c_void,
    winner: *mut c_void,
    direction: i64,
) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return;
    };
    let current = crate::scheduler::willow_sched_current_task();
    if current == 0 {
        return;
    }
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if raw != winner || direction != 0 {
            clear_wait(raw, &mut state.waiters, current, ChannelRole::RecvWait);
            clear_reservation(raw, &mut state.recv_claims, current, ChannelRole::RecvClaim);
        }
        if raw != winner || direction != 1 {
            clear_wait(raw, &mut state.send_waiters, current, ChannelRole::SendWait);
            clear_reservation(
                raw,
                &mut state.send_handoffs,
                current,
                ChannelRole::SendHandoff,
            );
        }
    }
    wake_recv_waiters(raw, channel);
    wake_send_waiters(raw, channel);
}

/// Metadata transitions occur under Channel -> TaskShard; wake and stress
/// collection occur after unlocking. Failed wakes undo only their generation.
fn wake_reserved_waiters(raw: *mut c_void, channel: &WillowAbiChannel, receive: bool) {
    loop {
        let candidate = {
            let mut state = channel.state.lock().expect("channel mutex poisoned");
            if receive {
                if state.values.len() <= state.recv_claims.len() {
                    break;
                }
            } else if state.closed || state_is_full(&state) {
                break;
            }
            let queue = if receive {
                &mut state.waiters
            } else {
                &mut state.send_waiters
            };
            let Some((task, wait)) = queue.pop_front_ticket() else {
                break;
            };
            let generation = next_generation(&mut state);
            let (wait_role, owned_role) = if receive {
                (ChannelRole::RecvWait, ChannelRole::RecvClaim)
            } else {
                (ChannelRole::SendWait, ChannelRole::SendHandoff)
            };
            let old = token(raw, wait_role, wait);
            let owner = token(raw, owned_role, generation);
            let map = if receive {
                &mut state.recv_claims
            } else {
                &mut state.send_handoffs
            };
            debug_assert!(!map.contains_key(&task));
            map.insert(task, generation);
            if crate::scheduler::transition_channel_ownership(task, old, owner) {
                Some((task, owner))
            } else {
                map.remove(&task);
                crate::scheduler::clear_channel_ownership(task, old);
                None
            }
        };
        if let Some((task, owner)) = candidate {
            #[cfg(test)]
            CHANNEL_WAKE_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !crate::scheduler::wake_channel_owner(task) {
                let mut state = channel.state.lock().expect("channel mutex poisoned");
                remove_exact(&mut state, task, owner);
            }
        }
    }
    if receive {
        wake_closed_empty(raw, channel);
    }
}

#[cfg(test)]
static CHANNEL_WAKE_ATTEMPTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn wake_recv_waiters(raw: *mut c_void, channel: &WillowAbiChannel) {
    wake_reserved_waiters(raw, channel, true);
}
fn wake_send_waiters(raw: *mut c_void, channel: &WillowAbiChannel) {
    wake_reserved_waiters(raw, channel, false);
}

fn drain_waiters(
    raw: *mut c_void,
    queue: &mut WaiterQueue,
    role: ChannelRole,
    wake: &mut HashSet<u64>,
) {
    while let Some((task, generation)) = queue.pop_front_ticket() {
        crate::scheduler::clear_channel_ownership(task, token(raw, role, generation));
        wake.insert(task);
    }
}

fn wake_closed_empty(raw: *mut c_void, channel: &WillowAbiChannel) {
    let mut wake = HashSet::new();
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if !state.closed || !state.values.is_empty() {
            return;
        }
        drain_waiters(raw, &mut state.waiters, ChannelRole::RecvWait, &mut wake);
    }
    for task in wake {
        crate::scheduler::willow_sched_wake(task);
    }
}

fn take_value(
    raw: *mut c_void,
    state: &mut WillowChannelState,
    current: u64,
) -> Option<WillowChannelValue> {
    if state.recv_claims.contains_key(&current) || state.values.len() > state.recv_claims.len() {
        clear_reservation(raw, &mut state.recv_claims, current, ChannelRole::RecvClaim);
        clear_wait(raw, &mut state.waiters, current, ChannelRole::RecvWait);
        state.values.pop_front()
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
                wake_send_waiters(raw, channel);
                wake_recv_waiters(raw, channel);
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
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return;
    };
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let mut wake = HashSet::new();
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        state.closed = true;
        channel.not_empty.notify_all();
        drain_waiters(
            raw,
            &mut state.send_waiters,
            ChannelRole::SendWait,
            &mut wake,
        );
        for (task, generation) in state.send_handoffs.drain() {
            crate::scheduler::clear_channel_ownership(
                task,
                token(raw, ChannelRole::SendHandoff, generation),
            );
            wake.insert(task);
        }
        if state.values.is_empty() {
            drain_waiters(raw, &mut state.waiters, ChannelRole::RecvWait, &mut wake);
        }
    }
    for task in wake {
        crate::scheduler::willow_sched_wake(task);
    }
    wake_recv_waiters(raw, channel);
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

    fn expected_test_ownership(task: u64, raw: *mut c_void) -> Vec<ChannelOwnershipToken> {
        let state = unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap();
        [
            (ChannelRole::RecvWait, state.waiters.ticket(task)),
            (
                ChannelRole::RecvClaim,
                state.recv_claims.get(&task).copied(),
            ),
            (ChannelRole::SendWait, state.send_waiters.ticket(task)),
            (
                ChannelRole::SendHandoff,
                state.send_handoffs.get(&task).copied(),
            ),
        ]
        .into_iter()
        .filter_map(|(role, generation)| generation.map(|g| token(raw, role, g)))
        .collect()
    }

    fn register_existing_test_ownership(task: u64, raw: *mut c_void) {
        for owner in expected_test_ownership(task, raw) {
            assert!(crate::scheduler::install_channel_ownership(task, owner));
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
    fn channel_unit_14_abi_recv_closed_empty_raises_and_returns_neutral_word() {
        let _heap = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let previous = crate::panic_context::replace_current_context(Some(std::sync::Arc::new(
            crate::panic_context::PanicContext::new(14),
        )));
        let ch = willow_channel_new(0);
        willow_channel_close(ch);
        assert_eq!(willow_channel_recv_i64(ch), 0);
        assert_eq!(
            crate::panic_context::willow_panic_active(),
            1,
            "the neutral ABI word must never be observed as a Willow value"
        );
        crate::panic_context::willow_panic_enter_defer();
        let recovered = crate::panic_context::willow_panic_recover();
        crate::panic_context::willow_panic_leave_defer();
        assert!(!recovered.is_null());
        crate::panic_context::willow_panic_release_recovered(recovered);
        crate::panic_context::replace_current_context(previous);
    }

    // willow-vynv.1: send wakes EVERY parked waiter (a cancelled head waiter
    // must not swallow the single wake and starve live consumers).
    #[test]
    fn send_reserves_one_value_and_wakes_one_receiver() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        let raw = willow_channel_new(0);
        let (first, second) = crate::scheduler::with_global_for_test(|s| {
            (s.spawn_parked_placeholder(), s.spawn_parked_placeholder())
        });
        for task in [first, second] {
            crate::scheduler::with_current_task_for_test(task, || {
                assert_eq!(willow_channel_recv_ready(raw), 0)
            });
        }
        CHANNEL_WAKE_ATTEMPTS.store(0, std::sync::atomic::Ordering::Relaxed);
        willow_channel_send_i64(raw, 42);
        let mut state = unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap();
        assert_eq!(state.recv_claims.len(), 1);
        assert!(state.recv_claims.contains_key(&first));
        assert_eq!(state.waiters.live(), vec![second]);
        assert!(take_value(raw, &mut state, 0).is_none());
        assert!(take_value(raw, &mut state, second).is_none());
        assert_eq!(
            unsafe { take_value(raw, &mut state, first).unwrap().i64_value },
            42
        );
        assert_eq!(
            CHANNEL_WAKE_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
            1
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
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
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
        let channel = unsafe { channel_from_raw(slot as *mut c_void) }.unwrap();
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
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
            let mut state = channel.state.lock().unwrap();
            // The duplicate registration must collapse: `register` is the only
            // way in, and it rejects an id already in the queue.
            for id in [t7, t9, t7] {
                state.waiters.register(id);
            }
            drop(state);
            register_existing_test_ownership(t7, raw);
        }

        purge_task(t7);

        for raw in [first, second] {
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
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
        unsafe { channel_from_raw(unregister_channel) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .waiters
            .register(unregister_task);
        register_existing_test_ownership(unregister_task, unregister_channel);
        crate::scheduler::with_global_for_test(|sched| sched.set_running(unregister_task));
        willow_channel_unregister_waiter(unregister_channel);
        crate::scheduler::with_global_for_test(|sched| sched.clear_running());
        assert!(
            crate::scheduler::take_channel_waits(unregister_task).is_empty(),
            "select unregister must remove the task-side channel address"
        );

        let send_channel = willow_channel_new(0);
        unsafe { channel_from_raw(send_channel) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .waiters
            .register(send_task);
        register_existing_test_ownership(send_task, send_channel);
        willow_channel_send_i64(send_channel, 1);
        let ownership = crate::scheduler::take_channel_waits(send_task);
        assert_eq!(ownership, expected_test_ownership(send_task, send_channel));
        assert_eq!(ownership[0].role, ChannelRole::RecvClaim);

        let close_channel = willow_channel_new(0);
        unsafe { channel_from_raw(close_channel) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .waiters
            .register(close_task);
        register_existing_test_ownership(close_task, close_channel);
        willow_channel_close(close_channel);
        assert!(
            crate::scheduler::take_channel_waits(close_task).is_empty(),
            "close wake must remove the task-side channel address"
        );

        crate::gc::willow_gc_collect();
    }

    // ── Bounded channels (willow-o038) ───────────────────────────────────────

    fn capacity_of(raw: *mut c_void) -> Option<usize> {
        unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .capacity
    }

    fn queued(raw: *mut c_void) -> usize {
        unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .values
            .len()
    }

    fn send_waiter_ids(raw: *mut c_void) -> Vec<u64> {
        unsafe { channel_from_raw(raw) }
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
        unsafe { channel_from_raw(ch) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .register(task);
        register_existing_test_ownership(task, ch);
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
            let mut state = unsafe { channel_from_raw(ch) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            state.send_waiters.register(first);
            state.send_waiters.register(second);
        }
        register_existing_test_ownership(first, ch);
        register_existing_test_ownership(second, ch);

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
            expected_test_ownership(first, ch),
            "the woken producer keeps a reverse reference until it sends or defects"
        );
        assert_eq!(
            crate::scheduler::take_channel_waits(second),
            expected_test_ownership(second, ch),
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
            expected_test_ownership(second, ch),
            "the replacement handoff remains cancellable until consumed"
        );
    }

    #[test]
    fn bounded_unit_10_cancelled_handoff_wakes_the_next_producer() {
        let _guard = crate::gc::runtime_test_guard();
        // The drive below must reap `first` and nothing else. Cancelling `first`
        // compensates the handoff and leaves `second` READY, and a worker pool
        // would race to claim and complete that placeholder before the run loop
        // notices its target is done (willow-tcrg).
        let _single_worker = crate::scheduler::single_worker_for_test();
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
            expected_test_ownership(second, ch)
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
        unsafe { channel_from_raw(ch) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .register(task);
        register_existing_test_ownership(task, ch);
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
        unsafe { channel_from_raw(ch) }
            .unwrap()
            .state
            .lock()
            .unwrap()
            .send_waiters
            .register(task);
        register_existing_test_ownership(task, ch);
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
            let mut state = unsafe { channel_from_raw(ch) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            state.send_waiters.register(stale);
            state.send_waiters.register(live);
        }
        // Terminal metadata refuses installation; queue entry deliberately remains stale.
        register_existing_test_ownership(live, ch);

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
            expected_test_ownership(live, ch),
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
        let state = unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap();
        assert_eq!(state.waiters.live(), vec![task]);
        assert_eq!(state.waiters.queued_entries(), 1);
        drop(state);

        crate::scheduler::with_global_for_test(|sched| sched.clear_running());
        assert_eq!(
            crate::scheduler::take_channel_waits(task),
            expected_test_ownership(task, raw),
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
        let state = unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap();
        assert_eq!(state.send_waiters.queued_entries(), 1);
        drop(state);

        crate::scheduler::with_global_for_test(|sched| sched.clear_running());
        assert_eq!(
            crate::scheduler::take_channel_waits(task),
            expected_test_ownership(task, raw)
        );
    }

    #[test]
    fn wq_13_purge_task_clears_both_queues() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let (victim, other) = crate::scheduler::with_global_for_test(|sched| {
            (sched.spawn_placeholder(), sched.spawn_parked_placeholder())
        });
        let raw = willow_channel_new_bounded(0, 1);
        assert_eq!(willow_channel_try_send_i64(raw, 1), 1);
        {
            let mut state = unsafe { channel_from_raw(raw) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            state.waiters.register(victim);
            state.waiters.register(other);
            state.send_waiters.register(victim);
            state.send_waiters.register(other);
        }
        register_existing_test_ownership(victim, raw);
        register_existing_test_ownership(other, raw);

        purge_task(victim);

        let state = unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap();
        assert!(state.waiters.is_empty());
        assert!(state.recv_claims.contains_key(&other));
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
            let mut state = unsafe { channel_from_raw(raw) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            state.waiters.register(task);
            state.send_waiters.register(task);
        }
        register_existing_test_ownership(task, raw);

        crate::scheduler::with_global_for_test(|sched| sched.set_running(task));
        willow_channel_unregister_waiter(raw);
        crate::scheduler::with_global_for_test(|sched| sched.clear_running());

        let state = unsafe { channel_from_raw(raw) }
            .unwrap()
            .state
            .lock()
            .unwrap();
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
            let mut state = unsafe { channel_from_raw(raw) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            state.waiters.register(task);
            state.send_waiters.register(task);
        }
        register_existing_test_ownership(task, raw);
        register_existing_test_ownership(task, other);

        willow_channel_close(raw);

        assert_eq!(
            crate::scheduler::take_channel_waits(task),
            expected_test_ownership(task, other),
            "close must drop only the closed channel's reverse reference"
        );
    }
    #[test]
    fn receive_select_retains_winner_and_compensates_losing_claim() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        let (owner, peer) = crate::scheduler::with_global_for_test(|s| {
            (s.spawn_parked_placeholder(), s.spawn_parked_placeholder())
        });
        let a = willow_channel_new(0);
        let b = willow_channel_new(0);
        for raw in [a, b] {
            crate::scheduler::with_current_task_for_test(owner, || {
                assert_eq!(willow_channel_recv_ready(raw), 0)
            });
        }
        crate::scheduler::with_current_task_for_test(peer, || {
            assert_eq!(willow_channel_recv_ready(b), 0)
        });
        willow_channel_send_i64(a, 1);
        // A wake already made owner runnable: readiness claims B directly.
        willow_channel_send_i64(b, 2);
        crate::scheduler::with_current_task_for_test(owner, || {
            // If B's queued wait lost the wake race, the peer owns B instead.
            willow_channel_select_cleanup(b, a, 0);
            willow_channel_select_cleanup(a, a, 0);
            assert_eq!(willow_channel_recv_i64(a), 1);
        });
        assert!(expected_test_ownership(owner, b).is_empty());
        crate::scheduler::with_current_task_for_test(peer, || {
            assert_eq!(willow_channel_recv_i64(b), 2)
        });
    }

    #[test]
    fn close_preserves_reserved_value_and_cancel_reoffers_it() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        let (first, second) = crate::scheduler::with_global_for_test(|s| {
            (s.spawn_parked_placeholder(), s.spawn_parked_placeholder())
        });
        let raw = willow_channel_new(0);
        for task in [first, second] {
            crate::scheduler::with_current_task_for_test(task, || {
                assert_eq!(willow_channel_recv_ready(raw), 0)
            });
        }
        willow_channel_send_i64(raw, 8);
        willow_channel_close(raw);
        {
            let mut state = unsafe { channel_from_raw(raw) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            assert!(take_value(raw, &mut state, 0).is_none());
            assert!(state.recv_claims.contains_key(&first));
        }
        purge_task(first);
        crate::scheduler::with_current_task_for_test(second, || {
            assert_eq!(willow_channel_recv_i64(raw), 8)
        });
    }

    #[test]
    fn stale_cleanup_cannot_erase_new_registration_or_other_role() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        let task = crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
        let raw = willow_channel_new_bounded(0, 1);
        let old;
        {
            let mut state = unsafe { channel_from_raw(raw) }
                .unwrap()
                .state
                .lock()
                .unwrap();
            assert!(register_wait(
                raw,
                &mut state.waiters,
                task,
                ChannelRole::RecvWait
            ));
            old = token(
                raw,
                ChannelRole::RecvWait,
                state.waiters.ticket(task).unwrap(),
            );
            clear_wait(raw, &mut state.waiters, task, ChannelRole::RecvWait);
            assert!(register_wait(
                raw,
                &mut state.waiters,
                task,
                ChannelRole::RecvWait
            ));
            assert!(register_wait(
                raw,
                &mut state.send_waiters,
                task,
                ChannelRole::SendWait
            ));
            remove_exact(&mut state, task, old);
        }
        assert!(!crate::scheduler::clear_channel_ownership(task, old));
        let owners = expected_test_ownership(task, raw);
        assert_eq!(owners.len(), 2);
        assert!(
            owners
                .iter()
                .any(|t| t.role == ChannelRole::RecvWait && t.generation != old.generation)
        );
        assert!(owners.iter().any(|t| t.role == ChannelRole::SendWait));
        assert_eq!(crate::scheduler::take_channel_waits(task), owners);
    }

    #[test]
    fn shared_channel_ten_and_hundred_thousand_sends_have_linear_wakes() {
        let _guard = crate::gc::runtime_test_guard();
        for count in [10_000usize, 100_000] {
            crate::scheduler::reset_global_scheduler_for_test();
            let raw = willow_channel_new(0);
            let tasks = crate::scheduler::with_global_for_test(|s| {
                (0..count)
                    .map(|_| s.spawn_parked_placeholder())
                    .collect::<Vec<_>>()
            });
            for &task in &tasks {
                crate::scheduler::with_current_task_for_test(task, || {
                    assert_eq!(willow_channel_recv_ready(raw), 0)
                });
            }
            CHANNEL_WAKE_ATTEMPTS.store(0, std::sync::atomic::Ordering::Relaxed);
            let started = std::time::Instant::now();
            for value in 0..count {
                willow_channel_send_i64(raw, value as i64);
            }
            let elapsed = started.elapsed();
            assert_eq!(
                CHANNEL_WAKE_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
                count
            );
            assert!(
                elapsed < std::time::Duration::from_secs(180),
                "{count} shared sends took {elapsed:?}"
            );
            for (value, &task) in tasks.iter().enumerate() {
                crate::scheduler::with_current_task_for_test(task, || {
                    assert_eq!(willow_channel_recv_i64(raw), value as i64)
                });
            }
            eprintln!("shared channel: {count} waiters/sends, {count} wake attempts, {elapsed:?}");
        }
        crate::scheduler::reset_global_scheduler_for_test();
    }
    #[test]
    fn same_channel_select_keeps_only_winning_direction() {
        let _guard = crate::gc::runtime_test_guard();
        for direction in [0, 1] {
            crate::scheduler::reset_global_scheduler_for_test();
            let task = crate::scheduler::with_global_for_test(|s| s.spawn_placeholder());
            let raw = willow_channel_new_bounded(0, 2);
            willow_channel_send_i64(raw, 4);
            crate::scheduler::with_current_task_for_test(task, || {
                assert_eq!(willow_channel_recv_ready(raw), 1);
                assert_eq!(willow_channel_send_ready(raw), 1);
                willow_channel_select_cleanup(raw, raw, direction);
                // Duplicate/aliased cleanup must be harmless.
                willow_channel_select_cleanup(raw, raw, direction);
                let owners = expected_test_ownership(task, raw);
                if direction == 0 {
                    assert_eq!(owners.len(), 1);
                    assert_eq!(owners[0].role, ChannelRole::RecvClaim);
                    assert_eq!(willow_channel_recv_i64(raw), 4);
                } else {
                    assert!(
                        !owners.iter().any(|t| matches!(
                            t.role,
                            ChannelRole::RecvWait | ChannelRole::RecvClaim
                        ))
                    );
                    assert_eq!(willow_channel_try_send_i64(raw, 5), 1);
                }
            });
        }
    }

    #[test]
    fn simultaneous_claims_release_only_losing_channel() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        let owner = crate::scheduler::with_global_for_test(|s| s.spawn_placeholder());
        let a = willow_channel_new(0);
        let b = willow_channel_new(0);
        willow_channel_send_i64(a, 11);
        willow_channel_send_i64(b, 22);
        crate::scheduler::with_current_task_for_test(owner, || {
            assert_eq!(willow_channel_recv_ready(a), 1);
            assert_eq!(willow_channel_recv_ready(b), 1);
            assert_eq!(
                expected_test_ownership(owner, a)[0].role,
                ChannelRole::RecvClaim
            );
            assert_eq!(
                expected_test_ownership(owner, b)[0].role,
                ChannelRole::RecvClaim
            );
            willow_channel_select_cleanup(b, a, 0);
            willow_channel_select_cleanup(a, a, 0);
            assert_eq!(willow_channel_recv_i64(a), 11);
        });
        assert_eq!(willow_channel_recv_i64(b), 22);
    }
}
