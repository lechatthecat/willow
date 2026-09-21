//! C ABI entry points for channels and select.

use super::*;

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
    wake_recv_waiters(channel);
    wake_send_waiters(channel);
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
    unsafe { crate::scheduler::willow_sched_wake_many(wake.as_ptr(), wake.len()) };
    wake_recv_waiters(channel);
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
