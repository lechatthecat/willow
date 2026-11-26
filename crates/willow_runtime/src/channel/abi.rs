//! C ABI entry points for channels and select.

use super::*;

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_channel_new(is_ref: i64) -> *mut c_void {
    ensure_channel_registered();
    let is_ref = is_ref != 0;
    const { assert!(align_of::<WillowAbiChannel>() <= align_of::<crate::gc::GcHeader>()) };
    // SAFETY: The constructor selects the matching payload layout and initializes its fields
    // before exposing it or reaching a GC point.
    let payload = unsafe {
        crate::gc::willow_alloc_with_layout(
            crate::gc::GcObjectKind::Channel,
            CHANNEL_TYPE_ID,
            std::mem::size_of::<WillowAbiChannel>() as i64,
            0,
        )
    };
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    // Placement-init into GC memory. `drop_channel` runs the Rust destructor
    // during sweep so the state-owned queue buffers are released too.
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    unsafe {
        (payload as *mut WillowAbiChannel).write(WillowAbiChannel::new(is_ref));
    }
    payload as *mut c_void
}

/// `Channel<T>::with_capacity(n)`: a BOUNDED channel — `send`
/// on a full buffer parks the producer until a `recv` frees space or the
/// channel closes. Capacity must be positive; rendezvous (capacity 0) is
/// explicitly unsupported in v1.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_channel_new_bounded(is_ref: i64, capacity: i64) -> *mut c_void {
    if capacity <= 0 {
        channel_raise_with(
            "channel capacity must be positive (rendezvous channels are not supported)",
        );
        return std::ptr::null_mut();
    }
    let raw = willow_channel_new(is_ref);
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    if let Some(channel) = unsafe { channel_from_raw(raw) } {
        channel
            .state
            .lock()
            .expect("channel mutex poisoned")
            .capacity = Some(capacity as usize);
    }
    raw
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_try_send_i64(raw: *mut c_void, value: i64) -> i32 {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { channel_try_send_value(raw, WillowChannelValue { i64_value: value }) }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_try_send_bool(raw: *mut c_void, value: u8) -> i32 {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { channel_try_send_value(raw, bool_channel_value(value)) }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_try_send_f64(raw: *mut c_void, value: f64) -> i32 {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { channel_try_send_value(raw, WillowChannelValue { f64_value: value }) }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_try_send_ptr(raw: *mut c_void, value: *mut c_void) -> i32 {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { channel_try_send_value(raw, WillowChannelValue { ptr_value: value }) }
}

/// Send-readiness probe for select send cases: 1 when not full
/// (or closed/unbounded); 0 after registering the running task as a send
/// waiter on a FULL bounded channel. A closed channel is ready so the arm is
/// chosen and its send raises `send on closed channel`, the same panic as a
/// plain `send`.
///
/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_send_ready(raw: *mut c_void) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 1;
    };
    crate::task::prepare_channel_wait_links();
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

/// Cooperative `recv` readiness probe: returns 1 if a value is
/// available OR the channel is closed (the caller then reads the value, or
/// observes the closed-empty state: `willow_channel_recv_*` raises it as a
/// language panic, `willow_channel_recv_opt_into` answers `None`);
/// returns 0 if the channel is empty and open,
/// after registering the currently-running task as a waiter — the caller's poll
/// fn then returns Pending and is woken by a later `send`/`close`.
///
/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_ready(raw: *mut c_void) -> i32 {
    let no_preempt = crate::preempt::NoPreemptGuard::enter();
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 1;
    };
    crate::task::prepare_channel_wait_links();
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    let current = crate::scheduler::willow_sched_current_task();
    if state.recv_claims.contains_key(&current) {
        return 1;
    }
    refill_from_inbox(channel, &mut state);
    if state.values.len() <= state.recv_claims.len() && !(state.closed && state.values.is_empty()) {
        // About to park: values sent since the refill join the queue first.
        close_inbox(channel, &mut state);
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
    park_receiver(raw, state, no_preempt, current)
}

/// Fused receive of a `recv` suspension, with one channel lock: returns 1
/// after writing the taken value's storage word to `out_payload`, 2 when the
/// channel is closed and drained (or null; the caller's ordinary receive then
/// raises or answers `None`), 0 after registering the running task as a
/// waiter, like `willow_channel_recv_ready`.
///
/// Same outcomes as `willow_channel_recv_ready` followed by the receive, minus
/// the receive claim it installs only to clear at once: nothing can run in
/// between, because the generated code has no safepoint there. A task with a
/// pending cancellation still takes no unreserved value. `select` keeps
/// `willow_channel_recv_ready`, since its probes must not consume.
///
/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. When a value is taken, out_payload must
/// exclusively address one aligned, writable i64 disjoint from the channel.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_take(raw: *mut c_void, out_payload: *mut i64) -> i32 {
    let no_preempt = crate::preempt::NoPreemptGuard::enter();
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return 2;
    };
    crate::task::prepare_channel_wait_links();
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    let current = crate::scheduler::willow_sched_current_task();
    if !state.recv_claims.contains_key(&current) {
        refill_from_inbox(channel, &mut state);
        if state.values.len() <= state.recv_claims.len()
            && !(state.closed && state.values.is_empty())
        {
            // About to park: values sent since the refill join the queue first.
            close_inbox(channel, &mut state);
        }
    }
    if current != 0
        && !state.recv_claims.contains_key(&current)
        && state.values.len() > state.recv_claims.len()
        && crate::scheduler::current_task_cancel_pending(current)
    {
        // `willow_channel_recv_ready` refuses its claim here: the value stays
        // queued and the cancelled task parks to be unwound.
        clear_wait(raw, &mut state.waiters, current, ChannelRole::RecvWait);
        return 0;
    }
    if let Some(value) = take_value(raw, &mut state, current) {
        open_inbox_if_idle(channel, &mut state);
        let value = finish_take(raw, channel, state, value);
        // SAFETY: The ABI caller provides a live, aligned, writable output slot for this result.
        unsafe { out_payload.write(value.i64_value) };
        return 1;
    }
    if state.closed && state.values.is_empty() {
        return 2;
    }
    park_receiver(raw, state, no_preempt, current)
}

/// Register the running task as a receive waiter of an empty open channel and
/// answer 0: the caller's poll returns Pending until a send or close wakes it.
fn park_receiver(
    raw: *mut c_void,
    mut state: crate::native_memory::MutexGuard<'_, WillowChannelState>,
    no_preempt: crate::preempt::NoPreemptGuard,
    current: u64,
) -> i32 {
    let registered = register_wait(raw, &mut state.waiters, current, ChannelRole::RecvWait);
    drop(state);
    drop(no_preempt);
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

/// Remove the currently-running task from this channel's waiter queue. A cooperative `select`
/// registers itself (via recv_ready) on
/// every recv channel while waiting; once it picks a case it must unregister
/// from all of them so a later send/close does not spuriously wake the
/// already-resumed task.
///
/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_unregister_waiter(raw: *mut c_void) {
    // SAFETY: The handle is null or the live channel retained for this operation.
    unsafe { willow_channel_select_cleanup(raw, std::ptr::null_mut(), -1) };
}

/// Called per channel arm by select, before committing its
/// winning operation. Direction: 0 recv, 1 send, -1 non-channel winner.
/// Repeated calls for aliased arms are harmless and retain the winning role.
///
/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_select_cleanup(
    raw: *mut c_void,
    winner: *mut c_void,
    direction: i64,
) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return;
    };
    // Only the native core may be borrowed across deferred-job allocation.
    let channel: &ChannelCore = channel;
    let current = crate::scheduler::willow_sched_current_task();
    if current == 0 {
        return;
    }
    let (wake_receivers, wake_senders) = {
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
        (
            !state.waiters.is_empty()
                && (state.values.len() > state.recv_claims.len()
                    || (state.closed && state.values.is_empty())),
            !state.send_waiters.is_empty() && !state.closed && !state_is_full(&state),
        )
    };
    // A later registration probes availability under this same mutex. Only
    // existing waiters can need compensation for the reservations released here.
    let raw = if wake_receivers {
        wake_waiters_bounded(raw, channel, true)
    } else {
        raw
    };
    if wake_senders {
        wake_waiters_bounded(raw, channel, false);
    }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_send_i64(raw: *mut c_void, value: i64) {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { willow_channel_send_value(raw, WillowChannelValue { i64_value: value }) };
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_send_bool(raw: *mut c_void, value: u8) {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { willow_channel_send_value(raw, bool_channel_value(value)) };
}

/// A bool payload with every byte initialized: `recv_opt` hands the whole
/// word out, and the compiler narrows it back to `bool`.
fn bool_channel_value(value: u8) -> WillowChannelValue {
    WillowChannelValue {
        i64_value: i64::from(value),
    }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_send_f64(raw: *mut c_void, value: f64) {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { willow_channel_send_value(raw, WillowChannelValue { f64_value: value }) };
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. For a reference payload the word must be null
/// or the current address of a live managed object of the declared element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_send_ptr(raw: *mut c_void, value: *mut c_void) {
    // SAFETY: The typed send supplies the live channel and matching union member; after a
    // collecting wait both the channel and any reference value are reloaded from roots.
    unsafe { willow_channel_send_value(raw, WillowChannelValue { ptr_value: value }) };
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_i64(raw: *mut c_void) -> i64 {
    // SAFETY: The channel element ABI supplies the initialized union representation read by this
    // operation.
    unsafe { willow_channel_recv_value(raw).i64_value }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_bool(raw: *mut c_void) -> u8 {
    // SAFETY: The channel element ABI supplies the initialized union representation read by this
    // operation.
    unsafe { willow_channel_recv_value(raw).bool_value }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_f64(raw: *mut c_void) -> f64 {
    // SAFETY: The channel element ABI supplies the initialized union representation read by this
    // operation.
    unsafe { willow_channel_recv_value(raw).f64_value }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_ptr(raw: *mut c_void) -> *mut c_void {
    // SAFETY: The channel element ABI supplies the initialized union representation read by this
    // operation.
    unsafe { willow_channel_recv_value(raw).ptr_value }
}

/// `Channel<T>::recv_opt()`: like `recv`, but a closed,
/// drained channel answers `None` instead of raising. Returns 0 (`Some`) after
/// writing the received storage word to `out_payload`, or 1 (`None`) — the
/// same tag/payload protocol as `willow_map_get_into`, so the compiler builds
/// the `Option<T>` with the same code.
///
/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait. Even for a null or closed channel, out_payload
/// must exclusively address one aligned, writable i64 disjoint from the channel and stable
/// across any wait.
///
/// ```compile_fail,E0133
/// let mut word = 0i64;
/// willow_runtime::channel::willow_channel_recv_opt_into(std::ptr::null_mut(), &mut word);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_recv_opt_into(
    raw: *mut c_void,
    out_payload: *mut i64,
) -> i64 {
    // SAFETY: The ABI caller provides a live, aligned, writable output slot for this result.
    unsafe { out_payload.write(0) };
    match channel_recv_or_closed(raw) {
        Some(value) => {
            // SAFETY: The ABI caller provides a live, aligned, writable output slot for this
            // result.
            unsafe { out_payload.write(value.i64_value) };
            0
        }
        None => 1,
    }
}

/// # Safety
/// A non-null raw handle must denote a live Willow channel from this runtime, kept reachable
/// through the operation, including any wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_channel_close(raw: *mut c_void) {
    // SAFETY: The channel constructor places WillowAbiChannel in word-aligned GC storage.
    let Some(channel) = (unsafe { channel_from_raw(raw) }) else {
        return;
    };
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let mut wake = HashSet::new();
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        close_inbox(channel, &mut state);
        state.closed = true;
        drain_waiters(
            channel,
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
            drain_waiters(
                channel,
                &mut state.waiters,
                ChannelRole::RecvWait,
                &mut wake,
            );
        }
    }
    let wake: Vec<_> = wake.into_iter().collect();
    // The native Vec remains valid across scheduler GC stress points.
    // SAFETY: The slice supplies exactly the indicated number of task IDs and remains live for the
    // call.
    unsafe { crate::scheduler::willow_sched_wake_many(wake.as_ptr(), wake.len()) };
    wake_recv_waiters(channel);
}

/// Monotonic seed for pseudo-random ready-case selection in `select`. Selection order is
/// pseudo-randomized to avoid SYSTEMATIC
/// source-order starvation; this is not a bounded-fairness guarantee.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
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
