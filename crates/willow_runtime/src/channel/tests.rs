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
    let _guard = crate::gc::runtime_test_guard();
    let ch = willow_channel_new(0);
    willow_channel_send_i64(ch, 10);
    willow_channel_send_i64(ch, 20);
    assert_eq!(willow_channel_recv_i64(ch), 10);
    assert_eq!(willow_channel_recv_i64(ch), 20);
}

#[test]
fn channel_unit_12_abi_bool_send_recv() {
    let _guard = crate::gc::runtime_test_guard();
    let ch = willow_channel_new(0);
    willow_channel_send_bool(ch, 1);
    assert_eq!(willow_channel_recv_bool(ch), 1);
}

#[test]
fn channel_unit_13_abi_f64_send_recv() {
    let _guard = crate::gc::runtime_test_guard();
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
fn gc_reset_drops_channel_native_owners_at_increasing_sizes() {
    let _guard = crate::gc::runtime_test_guard();
    crate::scheduler::reset_global_scheduler_for_test();
    crate::gc::reset_internal_for_test();
    for count in [1usize, 32, 256] {
        for queued in [1usize, 64] {
            let before = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst);
            for _ in 0..count {
                let raw = willow_channel_new(0);
                let channel = unsafe { channel_from_raw(raw) }.unwrap();
                let mut state = channel.state.lock().unwrap();
                for value in 0..queued {
                    state.values.push_back(WillowChannelValue {
                        i64_value: value as i64,
                    });
                    state.waiters.register(value as u64 + 1);
                }
            }
            crate::gc::reset_internal_for_test();
            let dropped = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst) - before;
            assert_eq!(
                dropped, count,
                "reset must destroy every native channel owner"
            );
            assert_eq!(crate::gc::willow_gc_allocated_bytes(), 0);
            crate::gc::reset_internal_for_test();
            assert_eq!(
                CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst) - before,
                count,
                "a repeated reset must not drop owners twice"
            );
            eprintln!(
                "reset channels={count} queued={queued} native_drops={dropped} live_owners=0"
            );
        }
    }
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

fn poison_channel(channel: &WillowAbiChannel) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _state = channel.state.lock().unwrap();
        panic!("intentional channel poison");
    }));
    assert!(result.is_err());
    assert!(channel.state.is_poisoned());
}

#[test]
fn poisoned_channel_gc_hooks_visit_each_queued_reference() {
    for count in [0, 1, 16, 256, 4096] {
        let mut channel = WillowAbiChannel::new(true);
        let mut values = vec![0u8; count];
        let expected: Vec<_> = values.iter_mut().map(|v| v as *mut u8).collect();
        {
            let mut state = channel.state.lock().unwrap();
            for &ptr in &expected {
                state.values.push_back(WillowChannelValue {
                    ptr_value: ptr.cast(),
                });
            }
            // Exercise a wrapped VecDeque without changing its length.
            for _ in 0..count / 2 {
                let value = state.values.pop_front().unwrap();
                state.values.push_back(value);
            }
        }
        let mut expected = expected;
        expected.rotate_left(count / 2);
        poison_channel(&channel);
        let payload = (&mut channel as *mut WillowAbiChannel).cast();
        let mut slots = Vec::new();
        unsafe { trace_channel(payload, &mut slots) };
        assert_eq!(slots.len(), count);
        assert_eq!(
            slots
                .iter()
                .map(|&slot| unsafe { *slot })
                .collect::<Vec<_>>(),
            expected
        );
        let mut children = vec![std::ptr::null_mut()];
        unsafe { snapshot_channel(payload, &mut children) };
        assert_eq!(&children[1..], expected);
        assert!(
            channel
                .state
                .try_lock()
                .is_err_and(|e| matches!(e, std::sync::TryLockError::Poisoned(_)))
        );
        channel.inner.is_ref = false;
        let payload = (&mut channel as *mut WillowAbiChannel).cast();
        slots.clear();
        children.clear();
        unsafe {
            trace_channel(payload, &mut slots);
            snapshot_channel(payload, &mut children);
        }
        assert!(slots.is_empty());
        assert!(children.is_empty());
    }
}

#[test]
fn poisoned_channel_keeps_queued_child_alive_during_collection() {
    let _guard = crate::gc::runtime_test_guard();
    crate::gc::reset_internal_for_test();
    let mut root = willow_channel_new(1).cast::<u8>();
    crate::gc::willow_push_root(&mut root);
    let child = willow_channel_new(0);
    willow_channel_send_value(root.cast(), WillowChannelValue { ptr_value: child });
    poison_channel(unsafe { &*root.cast::<WillowAbiChannel>() });
    let before = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst);
    crate::gc::willow_gc_collect();
    assert_eq!(
        CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "rooted poisoned channel and its queued child must both survive"
    );
    crate::gc::willow_pop_roots(1);
    crate::gc::willow_gc_collect();
    assert_eq!(
        CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        before + 2
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
    // the task on both queues; close must retain ownership on other channels.
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
    let other_owner = {
        let mut state = unsafe { channel_from_raw(other) }
            .unwrap()
            .state
            .lock()
            .unwrap();
        assert!(register_wait(
            other,
            &mut state.waiters,
            task,
            ChannelRole::RecvWait
        ));
        token(
            other,
            ChannelRole::RecvWait,
            state.waiters.ticket(task).unwrap(),
        )
    };
    let before = crate::scheduler::with_global_for_test(|sched| {
        sched
            .with_task_mut(task, |task| task.wait_channels().to_vec())
            .unwrap()
    });
    assert_eq!(
        before.len(),
        3,
        "both closed-channel roles and the other channel must be registered"
    );
    assert!(
        before.contains(&other_owner),
        "other ownership must exist before close"
    );

    willow_channel_close(raw);

    assert_eq!(
        crate::scheduler::take_channel_waits(task),
        vec![other_owner],
        "close must drop only the closed channel's reverse reference"
    );
    assert_eq!(expected_test_ownership(task, other), vec![other_owner]);
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
                    !owners
                        .iter()
                        .any(|t| matches!(t.role, ChannelRole::RecvWait | ChannelRole::RecvClaim))
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
#[test]
fn cancellation_tokens_follow_native_core_when_gc_wrappers_move() {
    let _guard = crate::gc::runtime_test_guard();
    for count in [1, 32, 256] {
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        let mut cases = Vec::new();
        for _ in 0..count {
            let task = crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
            let original = willow_channel_new(0);
            let destination = willow_channel_new(0);
            for raw in [original, destination] {
                let channel = unsafe { channel_from_raw(raw) }.unwrap();
                let mut state = channel.state.lock().unwrap();
                state.waiters.register(task);
                state.send_waiters.register(task);
                state.recv_claims.insert(task, 1);
                state.send_handoffs.insert(task, 1);
                drop(state);
                register_existing_test_ownership(task, raw);
            }
            let owners = expected_test_ownership(task, original);
            let decoys = expected_test_ownership(task, destination);
            assert_eq!(owners.len(), 4);
            assert_ne!(owners[0].channel, original as usize);
            // Model relocation of the GC wrapper without implementing or
            // enabling an evacuation protocol. Swap preserves both owners
            // and exactly-once native destruction at the existing GC slots.
            unsafe {
                std::ptr::swap(
                    original.cast::<WillowAbiChannel>(),
                    destination.cast::<WillowAbiChannel>(),
                )
            };
            assert_eq!(expected_test_ownership(task, destination), owners);
            assert_eq!(expected_test_ownership(task, original), decoys);
            cases.push((task, original, destination, owners, decoys));
        }
        for (task, original, destination, owners, decoys) in cases {
            purge_task_from_tokens(task, owners);
            assert!(expected_test_ownership(task, destination).is_empty());
            assert_eq!(expected_test_ownership(task, original), decoys);
            let mut remaining = crate::scheduler::take_channel_waits(task);
            // Reverse ownership is unordered; channel waiter queues own FIFO.
            remaining.sort_unstable();
            let mut expected = decoys;
            expected.sort_unstable();
            assert_eq!(remaining, expected);
            purge_task_from_tokens(task, remaining);
            assert!(expected_test_ownership(task, original).is_empty());
        }
        let before = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst);
        crate::gc::willow_gc_collect();
        let dropped = CHANNEL_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst) - before;
        assert_eq!(dropped, count * 2);
        println!(
            "moved_wrappers={} roles_cleaned={} native_drops={dropped}",
            count * 2,
            count * 8
        );
    }
    crate::scheduler::reset_global_scheduler_for_test();
    crate::gc::reset_internal_for_test();
}
