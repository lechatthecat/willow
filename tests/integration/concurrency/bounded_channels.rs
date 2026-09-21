use super::*;

// ── Bounded channels: `Channel<T>::with_capacity(n)` (willow-o038) ───────────
//
// A bounded channel's buffer holds at most `n` values. A send into a full
// buffer PARKS the producer (registering it as a send waiter) instead of
// growing the queue; a recv that frees a slot, or a close, wakes it. In
// `select`, a send case is ready only while the buffer has room.
//
// 26 test perspectives:
//   1. A send/recv round trip inside capacity works.
//   2. FIFO order is preserved under backpressure.
//   3. A fast producer + slow consumer delivers every value exactly once.
//   4. Capacity 1 (the minimum) works.
//   5. A capacity larger than the traffic behaves like an unbounded channel.
//   6. A producer parked on a full buffer resumes once the consumer drains.
//   7. Multiple producers into one bounded channel conserve the total.
//   8. `close` wakes a producer parked on a full buffer (no hang).
//   9. `send` after `close` is a no-op on a bounded channel.
//  10. `recv` after `close` still drains the buffered values.
//  11. `bool` elements.
//  12. `f64` elements.
//  13. `String` (GC pointer) elements.
//  14. GC stress with a parked producer keeps pointer elements live.
//  15. A `select` send case falls to `default` when the buffer is full.
//  16. That same send case becomes ready again after a `recv`.
//  17. An unbounded channel's select send case is always ready.
//  18. A select send case's value expression is evaluated exactly once.
//  19. A cooperative `ch.send(v)` evaluates its value once per send, not per park.
//  20. A select mixing a bounded send case and a recv case picks a ready one.
//  21. Bounded channels work from a synchronous `main` driving spawned tasks.
//  22. Cancelling a task parked on a full send leaves the channel usable.
//  23. Capacity 0 (rendezvous) is rejected at runtime with a clear message.
//  24. A negative capacity is rejected at runtime.
//  25. A non-`i64` capacity is a compile error.
//  26. Wrong argument / type-argument counts are compile errors.

#[test]
fn bch_01_round_trip_within_capacity() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::with_capacity(4); ch.send(7); println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn bch_02_fifo_order_under_backpressure() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { let mut i = 0; while i < 6 { ch.send(i); i = i + 1; } }\nasync fn main() { let ch = Channel<i64>::with_capacity(2); let p = produce(ch); let mut n = 0; while n < 6 { println(ch.recv()); n = n + 1; } await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n3\n4\n5\n");
}

#[test]
fn bch_03_fast_producer_slow_consumer_delivers_all() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { let mut i = 1; while i <= 10 { ch.send(i); i = i + 1; } }\nasync fn main() { let ch = Channel<i64>::with_capacity(2); let p = produce(ch); let mut total = 0; let mut n = 0; while n < 10 { total = total + ch.recv(); await sleep(2); n = n + 1; } await p; println(total); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "55\n");
}

#[test]
fn bch_04_capacity_one_is_valid() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { ch.send(1); ch.send(2); ch.send(3); }\nasync fn main() { let ch = Channel<i64>::with_capacity(1); let p = produce(ch); println(ch.recv() + ch.recv() + ch.recv()); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn bch_05_capacity_above_traffic_never_parks() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::with_capacity(100); ch.send(1); ch.send(2); ch.send(3); println(ch.recv() + ch.recv() + ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn bch_06_parked_producer_resumes_after_drain() {
    // The producer fills the 1-slot buffer, parks, and only finishes once the
    // consumer has drained both values.
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) -> i64 { ch.send(1); ch.send(2); return 9; }\nasync fn main() { let ch = Channel<i64>::with_capacity(1); let p = produce(ch); await sleep(30); println(ch.recv()); println(ch.recv()); println(await p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n9\n");
}

#[test]
fn bch_07_multiple_producers_conserve_total() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>, base: i64) { let mut i = 0; while i < 5 { ch.send(base + i); i = i + 1; } }\nasync fn main() { let ch = Channel<i64>::with_capacity(2); let a = produce(ch, 100); let b = produce(ch, 200); let mut total = 0; let mut n = 0; while n < 10 { total = total + ch.recv(); n = n + 1; } await a; await b; println(total); }",
    );
    assert!(ok, "{out}");
    // (100..104) + (200..204) = 510 + 1010
    assert_eq!(out, "1520\n");
}

#[test]
fn bch_08_close_wakes_parked_producer() {
    // The producer parks on a full buffer; `close` wakes it and its remaining
    // sends become no-ops, so it finishes instead of hanging.
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) -> i64 { let mut i = 0; while i < 50 { ch.send(i); i = i + 1; } return 7; }\nasync fn main() { let ch = Channel<i64>::with_capacity(1); let p = produce(ch); await sleep(20); ch.close(); println(await p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn bch_09_send_after_close_is_noop() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::with_capacity(2); ch.close(); ch.send(5); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn bch_10_recv_after_close_drains_buffer() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::with_capacity(2); ch.send(4); ch.send(5); ch.close(); println(ch.recv()); println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n5\n");
}

#[test]
fn bch_11_bool_elements() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<bool>) { ch.send(true); ch.send(false); ch.send(true); }\nasync fn main() { let ch = Channel<bool>::with_capacity(1); let p = produce(ch); println(ch.recv()); println(ch.recv()); println(ch.recv()); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

#[test]
fn bch_12_f64_elements() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<f64>) { ch.send(1.5); ch.send(2.25); }\nasync fn main() { let ch = Channel<f64>::with_capacity(1); let p = produce(ch); println(ch.recv() + ch.recv()); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3.75\n");
}

#[test]
fn bch_13_string_elements() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<String>) { ch.send(\"wil\"); ch.send(\"low\"); }\nasync fn main() { let ch = Channel<String>::with_capacity(1); let p = produce(ch); println(ch.recv() + ch.recv()); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

#[test]
fn bch_14_gc_stress_with_parked_producer() {
    // The producer parks holding a String in its frame slot; a collection while
    // it is parked must not free the queued or the pending value.
    let (out, ok) = compile_and_run_gc_stress(
        "async fn produce(ch: Channel<String>) { let mut i = 0; while i < 6 { ch.send(\"x\" + \"y\"); i = i + 1; } }\nasync fn main() { let ch = Channel<String>::with_capacity(1); let p = produce(ch); let mut s = \"\"; let mut n = 0; while n < 6 { s = s + ch.recv(); n = n + 1; } await p; println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xyxyxyxyxyxy\n");
}

#[test]
fn bch_15_select_send_case_falls_to_default_when_full() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::with_capacity(1); ch.send(1); select { ch.send(2) => { println(10); } default => { println(20); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

#[test]
fn bch_16_select_send_case_ready_again_after_recv() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::with_capacity(1); ch.send(1); select { ch.send(2) => { println(10); } default => { println(20); } } println(ch.recv()); select { ch.send(3) => { println(30); } default => { println(40); } } println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n1\n30\n3\n");
}

#[test]
fn bch_17_unbounded_select_send_always_ready() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); ch.send(1); ch.send(2); select { ch.send(3) => { println(10); } default => { println(20); } } println(ch.recv() + ch.recv() + ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n6\n");
}

#[test]
fn bch_18_select_send_value_evaluated_once() {
    // The value expression is evaluated at select ENTRY, before the readiness
    // probe — so it runs exactly once even when the send case loses to default.
    let (out, ok) = compile_and_run(
        "fn tick(v: i64) -> i64 { println(99); return v; }\nfn main() { let ch = Channel<i64>::with_capacity(1); ch.send(1); select { ch.send(tick(2)) => { println(10); } default => { println(20); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n20\n");
}

#[test]
fn bch_19_coop_send_value_evaluated_once_per_send() {
    // A cooperative send parks and re-enters its check block on each wakeup.
    // The value is frame-backed, so `tick` runs 3 times, not once per park.
    let (out, ok) = compile_and_run(
        "fn tick(v: i64) -> i64 { println(99); return v; }\nasync fn produce(ch: Channel<i64>) { let mut i = 1; while i <= 3 { ch.send(tick(i)); i = i + 1; } }\nasync fn main() { let ch = Channel<i64>::with_capacity(1); let p = produce(ch); await sleep(20); let mut total = 0; let mut n = 0; while n < 3 { total = total + ch.recv(); await sleep(5); n = n + 1; } await p; println(total); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n99\n99\n6\n");
}

#[test]
fn bch_20_select_mixes_bounded_send_and_recv() {
    let (out, ok) = compile_and_run(
        "fn main() { let full = Channel<i64>::with_capacity(1); full.send(1); let src = Channel<i64>::with_capacity(1); src.send(8); select { full.send(2) => { println(10); } let v = src.recv() => { println(v); } } }",
    );
    assert!(ok, "{out}");
    // Only the recv case can be ready: the send channel is full.
    assert_eq!(out, "8\n");
}

#[test]
fn bch_21_sync_main_drives_spawned_producer() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { let mut i = 1; while i <= 4 { ch.send(i); i = i + 1; } }\nfn main() { let ch = Channel<i64>::with_capacity(2); let p = produce(ch); let mut total = 0; let mut n = 0; while n < 4 { total = total + ch.recv(); n = n + 1; } println(total); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

#[test]
fn bch_22_cancel_parked_producer_leaves_channel_usable() {
    // Cancelling a task parked on a full send must purge its send-waiter
    // registration; the channel keeps working for a later producer.
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { let mut i = 0; while i < 50 { ch.send(i); i = i + 1; } }\nasync fn main() { let ch = Channel<i64>::with_capacity(1); let p = produce(ch); await sleep(20); p.cancel(); await sleep(20); println(p.is_cancelled()); println(ch.recv()); ch.send(42); println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n0\n42\n");
}

#[test]
fn bch_23_zero_capacity_rejected_at_runtime() {
    let (out, ok) = compile_and_run_check_exit(
        "fn main() { let ch = Channel<i64>::with_capacity(0); ch.send(1); }",
    );
    assert!(!ok, "{out}");
    assert!(out.contains("capacity must be positive"), "{out}");
}

#[test]
fn bch_24_negative_capacity_rejected_at_runtime() {
    let (out, ok) = compile_and_run_check_exit(
        "fn main() { let ch = Channel<i64>::with_capacity(-3); ch.send(1); }",
    );
    assert!(!ok, "{out}");
    assert!(out.contains("capacity must be positive"), "{out}");
}

#[test]
fn bch_25_non_i64_capacity_rejected() {
    assert_compile_error_contains(
        "fn main() { let ch = Channel<i64>::with_capacity(true); }\n",
        &["error[E0201]", "capacity must be `i64`"],
    );
}

#[test]
fn bch_26_wrong_argument_counts_rejected() {
    assert_compile_error_contains(
        "fn main() { let ch = Channel<i64>::with_capacity(); }\n",
        &["error[E0201]", "expects 1 argument"],
    );
    assert_compile_error_contains(
        "fn main() { let ch = Channel<i64, bool>::with_capacity(2); }\n",
        &["error[E0201]", "expects 1 type argument"],
    );
}
