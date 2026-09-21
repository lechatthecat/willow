use super::*;

// ── Return-position channel recv is a real suspend point (willow-0a6k.6) ────
// `return ch.recv();` used to fall into the SYNC recv path: it block-drove
// the scheduler from inside its own poll (nested run), could not park, could
// not be cancelled, and aborted 'recv would block' at idle. Now it suspends
// like let/assign/expr-position recv. 20 perspectives: 1 value delivery,
// 2 parks (no abort) + cancellable, 3 clean exit unawaited, 4 close() ->
// type default, 5 String channel, 6 f64 channel, 7 bool channel, 8 two
// sequential return-recv consumers, 9 return-recv task awaited through an
// async chain, 10 defer flushes on the return-recv exit, 11 mixed let+return
// positions in one fn, 12 producer/consumer roundtrip, 13 GC stress,
// 14 is_cancelled true after cancel while parked, 15 10s-idle cancel is
// prompt (no block-drive hang), 16 sync-fn return recv unchanged (value),
// 17 ordinary await returns the delivered value, 18 cancellation-aware await
// returns Err for a cancelled parked consumer, 19 send BEFORE first poll
// (buffered) still returns, 20 two
// channels, inner selected by arg.

#[test]
fn rrecv_01_value() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(42); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_02_parks_and_cancellable() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(20); h.cancel(); await sleep(20); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_03_clean_exit_unawaited() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(20); h.cancel(); await sleep(20); println(7); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn rrecv_04_close_wakes_then_closed_empty_panics() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.close(); println(await h); }",
    );
    assert!(!ok, "{out}");
    assert!(out.contains("recv on closed empty channel"), "{out}");
}

#[test]
fn rrecv_05_string_channel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<String>) -> String { return ch.recv(); }\nasync fn main() { let ch = Channel<String>::new(); let h = c(ch); await sleep(10); ch.send(\"hi\"); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi\n");
}

#[test]
fn rrecv_06_f64_channel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<f64>) -> f64 { return ch.recv(); }\nasync fn main() { let ch = Channel<f64>::new(); let h = c(ch); await sleep(10); ch.send(2.5); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2.5\n");
}

#[test]
fn rrecv_07_bool_channel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<bool>) -> bool { return ch.recv(); }\nasync fn main() { let ch = Channel<bool>::new(); let h = c(ch); await sleep(10); ch.send(true); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_08_two_sequential_consumers() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let a = c(ch); let b = c(ch); await sleep(10); ch.send(1); ch.send(2); println(await a + await b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn rrecv_09_through_async_chain() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn outer(ch: Channel<i64>) -> i64 { let v = await c(ch); return v * 2; }\nasync fn main() { let ch = Channel<i64>::new(); let h = outer(ch); await sleep(10); ch.send(21); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_10_defer_flushes() {
    let (out, ok) = compile_and_run(
        "fn cl() { println(1); }\nasync fn c(ch: Channel<i64>) -> i64 { defer cl(); return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(5); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n5\n");
}

#[test]
fn rrecv_11_mixed_positions() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { let a = ch.recv(); return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(1); ch.send(9); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn rrecv_12_producer_roundtrip() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { ch.send(3); ch.send(4); ch.close(); }\nasync fn consume(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let p = produce(ch); let a = consume(ch); let b = consume(ch); println(await a + await b); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn rrecv_13_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn c(ch: Channel<String>) -> String { return ch.recv(); }\nasync fn main() { let ch = Channel<String>::new(); let h = c(ch); await sleep(10); ch.send(\"a\" + \"b\"); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ab\n");
}

#[test]
fn rrecv_14_cancel_while_parked() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(30); h.cancel(); await sleep(30); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_15_idle_cancel_prompt() {
    // Nothing else runs: a block-driving recv would hang or abort here.
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); h.cancel(); await sleep(10); println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn rrecv_16_sync_fn_unchanged() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { ch.send(6); }\nfn take(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let p = produce(ch); println(take(ch)); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn rrecv_17_await_returns_delivered_value() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(8); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn rrecv_18_await_result_err_on_cancel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(20); h.cancel(); match await h.result() { Ok(v) => println(v), Err(e) => println(9), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn rrecv_19_send_before_first_poll() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); ch.send(55); let h = c(ch); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "55\n");
}

#[test]
fn rrecv_20_two_channels() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let x = Channel<i64>::new(); let y = Channel<i64>::new(); let a = c(x); let b = c(y); await sleep(10); x.send(1); y.send(2); println(await a); println(await b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

// Review fixes on the recv suspend arms (willow-0a6k.6): is_channel_recv
// matched by NAME only, so a user-defined `recv()` method was hijacked into
// the channel runtime (with an i64 element-type fallback). The arms are now
// gated on the receiver actually typing as Channel<T>. 21 user recv() in
// return position dispatches to the class method, 22 user recv() in let
// position, 23 user recv() in expr-stmt position, 24 channel recv unchanged
// alongside a user recv in the same program.

#[test]
fn rrecv_21_user_recv_return_position() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub base: i64; pub fn recv(self) -> i64 { return self.base + 1; } }\nasync fn f(x: Reader) -> i64 { return x.recv(); }\nasync fn main() { println(await f(new Reader(41))); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "user-defined recv() was treated as a channel:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_22_user_recv_let_position() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub base: i64; pub fn recv(self) -> i64 { return self.base * 2; } }\nasync fn f(x: Reader) -> i64 { let v = x.recv(); return v; }\nasync fn main() { println(await f(new Reader(5))); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "user-defined recv() was treated as a channel:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

#[test]
fn rrecv_23_user_recv_expr_position() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub n: i64; pub fn recv(self) -> i64 { println(self.n); return self.n; } }\nasync fn f(x: Reader) { x.recv(); }\nasync fn main() { await f(new Reader(7)); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "user-defined recv() was treated as a channel:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn rrecv_24_channel_and_user_recv_coexist() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub b: i64; pub fn recv(self) -> i64 { return self.b; } }\nasync fn c(ch: Channel<i64>, r: Reader) -> i64 { let a = ch.recv(); return a + r.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch, new Reader(2)); await sleep(10); ch.send(40); println(await h); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "mixed channel/user recv program hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_25_channel_consumer_example_is_repeatable_with_five_workers() {
    let source = fs::read_to_string("example/channel_consumer.wi").unwrap();
    let expected = "consumer 1 done\n42\nconsumer 2 done\ntrue\n";

    // The old example started both consumers before sending one value. With
    // five workers either waiter could consume it, leaving `await served`
    // parked forever. Repeat enough times to exercise parallel scheduling,
    // while bounding every run so a regression fails instead of hanging CI.
    for iteration in 1..=20 {
        let (out, ok, timed_out) = compile_and_run_with_env_timeout(
            &source,
            &[("WILLOW_WORKERS", "5")],
            std::time::Duration::from_secs(3),
        );
        assert!(
            !timed_out,
            "channel_consumer hung on iteration {iteration}:\n{out}"
        );
        assert!(
            ok,
            "channel_consumer failed on iteration {iteration}:\n{out}"
        );
        assert_eq!(out, expected, "iteration {iteration}");
    }
}

#[test]
fn rrecv_26_nested_arithmetic_and_call_argument_suspend() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "fn add(a: i64, b: i64) -> i64 { return a + b; }\nasync fn c(ch: Channel<i64>) -> i64 { return 1 + add(ch.recv(), 2) * 3; }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(4); println(await h); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "nested recv expression block-drove:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "19\n");
}

#[test]
fn rrecv_27_short_circuit_does_not_probe_unselected_recv() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "async fn c(ch: Channel<bool>) -> bool { return false && ch.recv(); }\nasync fn main() { let ch = Channel<bool>::new(); println(await c(ch)); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "short-circuited recv was evaluated:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn rrecv_28_ternary_only_suspends_selected_branch() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "async fn c(use_left: bool, left: Channel<i64>, right: Channel<i64>) -> i64 { return use_left ? left.recv() : right.recv(); }\nasync fn main() { let a = Channel<i64>::new(); let b = Channel<i64>::new(); let h = c(false, a, b); await sleep(10); b.send(8); println(await h); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "ternary probed the unselected channel:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn rrecv_29_while_condition_recv_is_rechecked_after_each_wake() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "async fn c(ch: Channel<i64>) -> i64 { let mut n = 0; while ch.recv() > 0 { n = n + 1; } return n; }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(1); ch.send(1); ch.send(0); println(await h); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "recv in while condition failed to resume:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn rrecv_30_await_inside_poll_parks_and_is_cancellable() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "async fn child(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn parent(ch: Channel<i64>) -> i64 { let h = child(ch); return 1 + await h; }\nasync fn main() { let ch = Channel<i64>::new(); let h = parent(ch); await sleep(20); h.cancel(); await sleep(20); println(h.is_cancelled()); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "await nested a scheduler run inside poll:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_31_select_inside_poll_parks_and_wakes() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "async fn choose(a: Channel<i64>, b: Channel<i64>) { select { let v = a.recv() => { println(v); } let v = b.recv() => { println(v + 1); } } }\nasync fn main() { let a = Channel<i64>::new(); let b = Channel<i64>::new(); let h = choose(a, b); await sleep(10); b.send(41); await h; }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "select block-drove the scheduler:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_32_parked_select_is_cancellable() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "async fn choose(a: Channel<i64>, b: Channel<i64>) { select { let v = a.recv() => { println(v); } let v = b.recv() => { println(v); } } }\nasync fn main() { let a = Channel<i64>::new(); let b = Channel<i64>::new(); let h = choose(a, b); await sleep(10); h.cancel(); await sleep(10); println(h.is_cancelled()); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "parked select could not be cancelled:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}
