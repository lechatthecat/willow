use super::*;

// ── Cancel runtime integrity (willow-vynv.1, defer Phase 1) ─────────────────
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
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>, tag: i64) -> i64 { let v = ch.recv(); println(tag); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let dead = consumer(ch, 1); await sleep(20); dead.cancel(); await sleep(20); let live = consumer(ch, 2); await sleep(20); ch.send(42); println(await live); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n42\n");
}

#[test]
fn cnl2_02_await_cancelled_is_located_panic() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn slow() -> i64 { await sleep(100); return 5; }\nasync fn main() { let t = slow(); t.cancel(); await sleep(20); println(await t); }",
    );
    assert!(!ok);
    assert!(out.contains("awaited a cancelled task"), "{out}");
}

#[test]
fn cnl2_03_cancel_recv_parked_exits_cleanly() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>) -> i64 { let v = ch.recv(); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let h = consumer(ch); await sleep(20); h.cancel(); await sleep(20); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cnl2_04_close_after_cancel_releases_live() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>, tag: i64) -> i64 { let v = ch.recv(); println(tag); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let dead = consumer(ch, 1); await sleep(20); dead.cancel(); await sleep(20); let live = consumer(ch, 2); await sleep(20); ch.send(0); ch.close(); println(await live); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n0\n");
}

#[test]
fn cnl2_05_cancelled_consumer_side_effects_never_run() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>) { let v = ch.recv(); println(999); }\nasync fn main() { let ch = Channel<i64>::new(); let h = consumer(ch); await sleep(20); h.cancel(); await sleep(20); ch.send(1); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn cnl2_06_value_survives_for_later_recv() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>) { ch.recv(); println(9); }\nasync fn main() { let ch = Channel<i64>::new(); let h = consumer(ch); await sleep(20); h.cancel(); await sleep(20); ch.send(77); await sleep(20); println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\n");
}
