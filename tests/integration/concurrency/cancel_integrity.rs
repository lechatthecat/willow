use super::*;

// ── Cancel runtime integrity (cancellation integrity, defer Phase 1) ─────────────────
// Integration half; unit half lives in the runtime crate (finalize wakes
// parked awaiters + claims them, wake no-ops on Cancelled, stale timer entry
// revalidated away, netpoll purge_task own/shared-fd, channel send drains all
// waiters). 6 e2e perspectives: 1 cancelled recv-parked consumer does not
// swallow the wake — a live consumer still receives (the lost-wakeup bug),
// 2 await on a cancelled task is a located panic (not garbage from the
// result slot), 3 program with a task cancelled while parked on recv exits
// cleanly, 4 close() after a cancel still releases live consumers,
// 5 cancelled consumer's post-recv side effects never run, 6 send to a
// queue holding ONLY a cancelled waiter keeps the value for a later recv.

#[test]
fn cnl2_01_cancelled_consumer_no_lost_wakeup() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn consumer(ch: Channel<i64>, ready: Channel<i64>, tag: i64) -> i64 { ready.send(test_current_task()); let v = ch.recv(); println(tag); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let ready = Channel<i64>::new(); let dead = consumer(ch, ready, 1); let dead_id = ready.recv(); while !test_task_parked(dead_id) { await yield(); } dead.cancel(); await dead.result(); let live = consumer(ch, ready, 2); let live_id = ready.recv(); while !test_task_parked(live_id) { await yield(); } ch.send(42); println(await live); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n42\n");
}

#[test]
fn cnl2_02_await_cancelled_is_located_panic() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn slow(gate: Channel<i64>) -> i64 { gate.recv(); return 5; }\nasync fn main() { let gate = Channel<i64>::new(); let t = slow(gate); t.cancel(); await t.result(); println(await t); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(!ok);
    assert!(out.contains("awaited a cancelled task"), "{out}");
}

#[test]
fn cnl2_03_cancel_recv_parked_exits_cleanly() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn consumer(ch: Channel<i64>, ready: Channel<i64>) -> i64 { ready.send(test_current_task()); let v = ch.recv(); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let ready = Channel<i64>::new(); let h = consumer(ch, ready); let id = ready.recv(); while !test_task_parked(id) { await yield(); } h.cancel(); await h.result(); println(h.is_cancelled()); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cnl2_04_close_after_cancel_releases_live() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn consumer(ch: Channel<i64>, ready: Channel<i64>, tag: i64) -> i64 { ready.send(test_current_task()); let v = ch.recv(); println(tag); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let ready = Channel<i64>::new(); let dead = consumer(ch, ready, 1); let dead_id = ready.recv(); while !test_task_parked(dead_id) { await yield(); } dead.cancel(); await dead.result(); let live = consumer(ch, ready, 2); let live_id = ready.recv(); while !test_task_parked(live_id) { await yield(); } ch.send(0); ch.close(); println(await live); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n0\n");
}

#[test]
fn cnl2_05_cancelled_consumer_side_effects_never_run() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn consumer(ch: Channel<i64>, ready: Channel<i64>) { ready.send(test_current_task()); let v = ch.recv(); println(999); }\nasync fn main() { let ch = Channel<i64>::new(); let ready = Channel<i64>::new(); let h = consumer(ch, ready); let id = ready.recv(); while !test_task_parked(id) { await yield(); } h.cancel(); await h.result(); ch.send(1); println(0); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn cnl2_06_value_survives_for_later_recv() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn consumer(ch: Channel<i64>, ready: Channel<i64>) { ready.send(test_current_task()); ch.recv(); println(9); }\nasync fn main() { let ch = Channel<i64>::new(); let ready = Channel<i64>::new(); let h = consumer(ch, ready); let id = ready.recv(); while !test_task_parked(id) { await yield(); } h.cancel(); await h.result(); ch.send(77); println(ch.recv()); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\n");
}
