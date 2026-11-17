//! Producer inbox of unbounded channels and the fused `recv` take.

use super::*;
use std::sync::atomic::Ordering;

const CHANNEL_OWNERSHIP_HASHES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelOwnershipHashes);
const CHANNEL_OWNERSHIP_SCANS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ChannelOwnershipScans);

fn core<'a>(raw: *mut c_void) -> &'a WillowAbiChannel {
    unsafe { channel_from_raw(raw) }.unwrap()
}

fn take(raw: *mut c_void) -> (i32, i64) {
    let mut payload = -1;
    let status = willow_channel_recv_take(raw, &mut payload);
    (status, payload)
}

fn inbox_len(raw: *mut c_void) -> usize {
    core(raw).inbox.lock().unwrap().values.len()
}

fn inbox_open(raw: *mut c_void) -> bool {
    let state = core(raw).state.lock().unwrap();
    let open = state.inbox_open;
    assert_eq!(
        core(raw).inbox.lock().unwrap().open,
        open,
        "state mirror and inbox flag agree"
    );
    open
}

/// One send and one take leave an idle unbounded channel with an open inbox.
fn opened_channel(is_ref: i64) -> *mut c_void {
    let raw = willow_channel_new(is_ref);
    assert!(!inbox_open(raw), "a new channel starts with a closed inbox");
    willow_channel_send_i64(raw, 0);
    assert_eq!(take(raw), (1, 0));
    assert!(inbox_open(raw));
    raw
}

#[test]
fn inbox_01_take_keeps_fifo_across_queue_and_inbox() {
    let _guard = crate::gc::isolated_runtime();
    let raw = willow_channel_new(0);
    for value in 1..=3 {
        willow_channel_send_i64(raw, value);
    }
    assert_eq!(inbox_len(raw), 0, "closed inbox: sends join the queue");
    assert_eq!(take(raw), (1, 1));
    for value in 4..=6 {
        willow_channel_send_i64(raw, value);
    }
    assert_eq!(inbox_len(raw), 3, "open inbox: sends skip the state lock");
    for expected in 2..=6 {
        assert_eq!(take(raw), (1, expected));
    }
}

#[test]
fn inbox_02_a_batch_is_one_inbox_acquisition_for_any_size() {
    let _guard = crate::gc::isolated_runtime();
    for count in [1_i64, 64, 4096] {
        let raw = opened_channel(0);
        for value in 0..count {
            willow_channel_send_i64(raw, value);
        }
        CHANNEL_INBOX_BATCHES.store(0, Ordering::Relaxed);
        for value in 0..count {
            assert_eq!(take(raw), (1, value));
        }
        let batches = CHANNEL_INBOX_BATCHES.load(Ordering::Relaxed);
        assert_eq!(batches, 1, "count={count}");
        println!("inbox count={count} batches={batches}");
    }
}

#[test]
fn inbox_03_slow_path_sender_absorbs_the_inbox_first() {
    let _guard = crate::gc::isolated_runtime();
    let raw = opened_channel(0);
    willow_channel_send_i64(raw, 1);
    willow_channel_send_i64(raw, 2);
    {
        // A sender that misses the hint takes the state lock: it must queue
        // behind the values already in the inbox.
        let channel = core(raw);
        channel.inbox_hint.store(false, Ordering::Relaxed);
    }
    willow_channel_send_i64(raw, 3);
    assert_eq!(inbox_len(raw), 0);
    for expected in 1..=3 {
        assert_eq!(take(raw), (1, expected));
    }
}

#[test]
fn inbox_04_close_drains_the_inbox_then_reports_closed() {
    let _guard = crate::gc::isolated_runtime();
    let raw = opened_channel(0);
    willow_channel_send_i64(raw, 7);
    willow_channel_send_i64(raw, 8);
    willow_channel_close(raw);
    assert!(!inbox_open(raw));
    assert_eq!(inbox_len(raw), 0, "closed inbox is empty");
    // A closed channel completes the try-send without queueing (the generated
    // send then raises).
    assert_eq!(willow_channel_try_send_i64(raw, 9), 1);
    assert_eq!(inbox_len(raw), 0);
    assert_eq!(take(raw), (1, 7));
    assert_eq!(take(raw), (1, 8));
    assert_eq!(take(raw).0, 2);
    assert!(!inbox_open(raw), "a closed channel never reopens its inbox");
}

#[test]
fn inbox_05_null_channel_take_reports_closed() {
    assert_eq!(take(std::ptr::null_mut()).0, 2);
}

#[test]
fn inbox_06_bounded_channels_never_open_the_inbox() {
    let _guard = crate::gc::isolated_runtime();
    let raw = willow_channel_new_bounded(0, 4);
    for round in 0..8 {
        assert_eq!(willow_channel_try_send_i64(raw, round), 1);
        assert_eq!(take(raw), (1, round));
        assert!(!inbox_open(raw));
        assert!(!core(raw).inbox_hint.load(Ordering::Relaxed));
    }
}

#[test]
fn inbox_07_parked_receiver_closes_the_inbox_and_a_send_wakes_it() {
    let _guard = crate::gc::isolated_runtime();
    crate::scheduler::reset_global_scheduler_for_test();
    let raw = opened_channel(0);
    let task = crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
    crate::scheduler::with_current_task_for_test(task, || assert_eq!(take(raw).0, 0));
    assert!(!inbox_open(raw), "a waiter forbids the inbox");
    assert_eq!(core(raw).state.lock().unwrap().waiters.live(), vec![task]);

    CHANNEL_WAKE_ATTEMPTS.store(0, Ordering::Relaxed);
    willow_channel_send_i64(raw, 9);
    assert_eq!(CHANNEL_WAKE_ATTEMPTS.load(Ordering::Relaxed), 1);
    assert!(
        core(raw)
            .state
            .lock()
            .unwrap()
            .recv_claims
            .contains_key(&task)
    );
    crate::scheduler::with_current_task_for_test(task, || assert_eq!(take(raw), (1, 9)));
    assert!(crate::scheduler::take_channel_waits(task).is_empty());
    assert!(inbox_open(raw), "idle again: the inbox reopens");
}

#[test]
fn inbox_08_reserved_value_is_kept_for_its_owner() {
    let _guard = crate::gc::isolated_runtime();
    crate::scheduler::reset_global_scheduler_for_test();
    let raw = opened_channel(0);
    let (owner, other) = crate::scheduler::with_global_for_test(|s| {
        (s.spawn_parked_placeholder(), s.spawn_parked_placeholder())
    });
    crate::scheduler::with_current_task_for_test(owner, || assert_eq!(take(raw).0, 0));
    willow_channel_send_i64(raw, 5);
    crate::scheduler::with_current_task_for_test(other, || assert_eq!(take(raw).0, 0));
    assert_eq!(
        take(raw).0,
        0,
        "an untasked caller parks behind the claim too"
    );
    crate::scheduler::with_current_task_for_test(owner, || assert_eq!(take(raw), (1, 5)));
}

#[test]
fn inbox_09_cancel_pending_task_takes_no_unreserved_value() {
    let _guard = crate::gc::isolated_runtime();
    crate::scheduler::reset_global_scheduler_for_test();
    let raw = opened_channel(0);
    let task = crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
    willow_channel_send_i64(raw, 4);
    crate::scheduler::willow_sched_cancel(task);
    crate::scheduler::with_current_task_for_test(task, || {
        assert_eq!(willow_channel_recv_ready(raw), 0, "the probe refuses");
        assert_eq!(take(raw).0, 0, "the fused take refuses alike");
    });
    assert!(core(raw).state.lock().unwrap().waiters.live().is_empty());
    assert!(crate::scheduler::take_channel_waits(task).is_empty());
    assert_eq!(take(raw), (1, 4), "the value stays queued for others");
}

#[test]
fn inbox_10_select_probe_sees_inbox_values() {
    let _guard = crate::gc::isolated_runtime();
    crate::scheduler::reset_global_scheduler_for_test();
    let raw = opened_channel(0);
    willow_channel_send_i64(raw, 3);
    assert_eq!(inbox_len(raw), 1);
    let task = crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
    crate::scheduler::with_current_task_for_test(task, || {
        assert_eq!(willow_channel_recv_ready(raw), 1);
        willow_channel_select_cleanup(raw, raw, 0);
        assert_eq!(willow_channel_recv_i64(raw), 3);
    });
    assert!(crate::scheduler::take_channel_waits(task).is_empty());
}

#[test]
fn inbox_11_takes_with_queued_values_touch_no_task_ownership() {
    let _guard = crate::gc::isolated_runtime();
    crate::scheduler::reset_global_scheduler_for_test();
    let raw = willow_channel_new(0);
    let idle = willow_channel_new(0);
    let task = crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
    // A parked wait on another channel gives the task an ownership entry, so
    // every ownership update below would have to look it up.
    crate::scheduler::with_current_task_for_test(task, || {
        assert_eq!(willow_channel_recv_ready(idle), 0);
    });
    for count in [16_i64, 256, 4096] {
        for value in 0..count {
            willow_channel_send_i64(raw, value);
        }
        CHANNEL_OWNERSHIP_HASHES.store(0, Ordering::Relaxed);
        CHANNEL_OWNERSHIP_SCANS.store(0, Ordering::Relaxed);
        crate::scheduler::with_current_task_for_test(task, || {
            for value in 0..count {
                assert_eq!(take(raw), (1, value));
            }
        });
        let lookups = CHANNEL_OWNERSHIP_HASHES.load(Ordering::Relaxed)
            + CHANNEL_OWNERSHIP_SCANS.load(Ordering::Relaxed);
        assert_eq!(lookups, 0, "count={count}");

        // The two-step probe and receive updates ownership per value.
        for value in 0..count {
            willow_channel_send_i64(raw, value);
        }
        crate::scheduler::with_current_task_for_test(task, || {
            for value in 0..count {
                assert_eq!(willow_channel_recv_ready(raw), 1);
                assert_eq!(willow_channel_recv_i64(raw), value);
            }
        });
        let probe_lookups = CHANNEL_OWNERSHIP_HASHES.load(Ordering::Relaxed)
            + CHANNEL_OWNERSHIP_SCANS.load(Ordering::Relaxed);
        assert!(probe_lookups >= count as usize, "count={count}");
        println!("take count={count} ownership_lookups={lookups} probe={probe_lookups}");
    }
}

fn boxed(value: i64) -> *mut c_void {
    let object = crate::gc::willow_alloc_object(43, 8);
    unsafe { *object.cast::<i64>() = value };
    object.cast()
}

/// Native bytes the channel state and inbox currently own.
fn buffer_capacities(raw: *mut c_void) -> (usize, usize) {
    let state = core(raw).state.lock().unwrap();
    let inbox = core(raw).inbox.lock().unwrap();
    (state.values.capacity(), inbox.values.capacity())
}

#[test]
fn inbox_12_gc_trace_and_snapshots_read_inbox_values_in_place() {
    let _guard = crate::gc::isolated_runtime();
    crate::gc::reset_internal_for_test();
    let raw = willow_channel_new(1);
    let first = boxed(1);
    willow_channel_send_ptr(raw, first);
    assert_eq!(willow_channel_recv_ptr(raw), first);
    assert!(inbox_open(raw));
    let queued = boxed(2);
    {
        // One value in `values` (sent while the inbox was closed) and the
        // rest in the inbox: the GC sees the logical queue in order.
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        close_inbox(channel, &mut state);
    }
    willow_channel_send_ptr(raw, queued);
    {
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        open_inbox_if_idle(channel, &mut state);
    }
    let values: Vec<_> = (3..6).map(boxed).collect();
    for &value in &values {
        willow_channel_send_ptr(raw, value);
    }
    assert_eq!(inbox_len(raw), values.len());
    let mut expected: Vec<*mut u8> = vec![queued.cast()];
    expected.extend(values.iter().map(|value| value.cast::<u8>()));
    let capacities = buffer_capacities(raw);

    let mut slots: Vec<*mut *mut u8> = Vec::new();
    unsafe { trace_channel(raw.cast(), &mut slots) };
    let traced: Vec<*mut u8> = slots.iter().map(|&slot| unsafe { *slot }).collect();
    assert_eq!(traced, expected, "STW trace covers the inbox in place");

    let mut children = Vec::new();
    unsafe { snapshot_channel(raw.cast(), &mut children) };
    assert_eq!(children, expected);

    let mut children = Vec::new();
    let progress = unsafe { snapshot_channel_slice(raw.cast(), 0, 8, &mut children) };
    assert!(matches!(progress, crate::gc::TraceSliceProgress::Done));
    children.sort_unstable();
    let mut sorted = expected.clone();
    sorted.sort_unstable();
    assert_eq!(children, sorted, "the first slice copies the inbox");

    // No GC path moved a value or grew a buffer: native accounting, whose
    // recharge can take the heap lock a stopped-world collector holds, is
    // untouched.
    assert_eq!(inbox_len(raw), values.len());
    assert_eq!(buffer_capacities(raw), capacities);
    for expected in std::iter::once(queued).chain(values) {
        assert_eq!(willow_channel_recv_ptr(raw), expected);
    }
}

#[test]
fn inbox_12b_slice_scan_skips_inbox_values_absorbed_after_the_first_slice() {
    let _guard = crate::gc::isolated_runtime();
    crate::gc::reset_internal_for_test();
    let raw = willow_channel_new(1);
    let opener = boxed(0);
    willow_channel_send_ptr(raw, opener);
    assert_eq!(willow_channel_recv_ptr(raw), opener);
    let head: Vec<_> = (1..5).map(boxed).collect();
    {
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        close_inbox(channel, &mut state);
    }
    for &value in &head {
        willow_channel_send_ptr(raw, value);
    }
    {
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        open_inbox_if_idle(channel, &mut state);
    }
    let tail = boxed(9);
    willow_channel_send_ptr(raw, tail);
    let mut children = Vec::new();
    let progress = unsafe { snapshot_channel_slice(raw.cast(), 0, 1, &mut children) };
    assert!(matches!(
        progress,
        crate::gc::TraceSliceProgress::Continue(1)
    ));
    {
        // A receive-side absorb between slices appends past the scan window.
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        absorb_inbox(channel, &mut state);
    }
    let mut cursor = 1;
    loop {
        let before = children.len();
        let progress = unsafe { snapshot_channel_slice(raw.cast(), cursor, 1, &mut children) };
        assert!(
            children.len() - before <= 1,
            "a slice stays within its budget"
        );
        match progress {
            crate::gc::TraceSliceProgress::Continue(next) => cursor = next,
            crate::gc::TraceSliceProgress::Done => break,
            crate::gc::TraceSliceProgress::Retry => panic!("no contention"),
        }
    }
    let mut expected: Vec<*mut u8> = head.iter().map(|value| value.cast()).collect();
    expected.push(tail.cast());
    children.sort_unstable();
    expected.sort_unstable();
    assert_eq!(children, expected, "each queued value exactly once");
}

#[test]
fn inbox_12c_slice_budget_holds_across_the_queue_and_the_inbox() {
    let _guard = crate::gc::isolated_runtime();
    crate::gc::reset_internal_for_test();
    let raw = willow_channel_new(1);
    let opener = boxed(0);
    willow_channel_send_ptr(raw, opener);
    assert_eq!(willow_channel_recv_ptr(raw), opener);
    let head: Vec<_> = (1..11).map(boxed).collect();
    {
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        close_inbox(channel, &mut state);
    }
    for &value in &head {
        willow_channel_send_ptr(raw, value);
    }
    {
        let channel = core(raw);
        let mut state = channel.state.lock().unwrap();
        open_inbox_if_idle(channel, &mut state);
    }
    let tail: Vec<_> = (11..101).map(boxed).collect();
    for &value in &tail {
        willow_channel_send_ptr(raw, value);
    }
    assert_eq!(inbox_len(raw), tail.len());
    let limit = 7;
    let mut children = Vec::new();
    let mut cursor = 0;
    let mut received = Vec::new();
    let mut crossed_into_inbox = false;
    loop {
        let before = children.len();
        let queued = core(raw).state.lock().unwrap().values.len();
        let progress = unsafe { snapshot_channel_slice(raw.cast(), cursor, limit, &mut children) };
        if cursor > 0 && children.len() - before > queued.saturating_sub(cursor - received.len()) {
            crossed_into_inbox = true;
        }
        assert!(
            children.len() - before <= limit,
            "a slice stays within its budget"
        );
        match progress {
            crate::gc::TraceSliceProgress::Continue(next) => cursor = next,
            crate::gc::TraceSliceProgress::Done => break,
            crate::gc::TraceSliceProgress::Retry => panic!("no contention"),
        }
        // Receives between slices pop at the front of the logical queue.
        received.push(willow_channel_recv_ptr(raw));
    }
    assert!(
        crossed_into_inbox,
        "a slice spanned the queue and the inbox"
    );
    let mut seen: Vec<*mut u8> = children.clone();
    seen.extend(received.iter().map(|value| value.cast::<u8>()));
    seen.sort_unstable();
    seen.dedup();
    let mut expected: Vec<*mut u8> = head.iter().chain(&tail).map(|value| value.cast()).collect();
    expected.sort_unstable();
    assert_eq!(seen, expected, "every queued value is scanned or received");
    let mut scanned = children.clone();
    scanned.sort_unstable();
    scanned.dedup();
    assert_eq!(scanned.len(), children.len(), "no value is scanned twice");
}

#[test]
fn inbox_13_slice_snapshot_retries_while_a_producer_holds_the_inbox() {
    let _guard = crate::gc::isolated_runtime();
    crate::gc::reset_internal_for_test();
    let raw = willow_channel_new(1);
    let value = boxed(1);
    willow_channel_send_ptr(raw, value);
    assert_eq!(willow_channel_recv_ptr(raw), value);
    let mut payload = 0;
    willow_channel_send_ptr(raw, value);
    assert_eq!(willow_channel_recv_take(raw, &mut payload), 1);
    willow_channel_send_ptr(raw, value);
    let held = core(raw).inbox.lock().unwrap();
    let mut children = Vec::new();
    let progress = unsafe { snapshot_channel_slice(raw.cast(), 0, 8, &mut children) };
    assert!(matches!(progress, crate::gc::TraceSliceProgress::Retry));
    assert!(children.is_empty());
    drop(held);
    let progress = unsafe { snapshot_channel_slice(raw.cast(), 0, 8, &mut children) };
    assert!(matches!(progress, crate::gc::TraceSliceProgress::Done));
    assert_eq!(children, vec![value.cast::<u8>()]);
}

#[test]
fn inbox_14_footprint_counts_the_inbox_buffer() {
    let mut inbox = Inbox {
        values: VecDeque::with_capacity(32),
        open: false,
    };
    let capacity = inbox.values.capacity();
    assert_eq!(
        crate::native_memory::Footprint::native_bytes(&mut inbox),
        capacity * std::mem::size_of::<WillowChannelValue>()
    );
}
