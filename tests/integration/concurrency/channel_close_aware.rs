use super::*;

// ---------------------------------------------------------------------------
// Close-aware channel receive (willow-jz15.44): `Channel<T>::recv_opt()`
// returns `Option<T>` — `Some(v)` while values remain, `None` once the channel
// is closed AND drained — and `let v = ch.recv_opt() =>` is the select form.
// Perspectives covered here:
//   1. an async "drain until closed" loop ends without a sentinel value
//   2. a sync fn drains buffered values, then sees `None`
//   3. values buffered before `close` are delivered in FIFO order first
//   4. `None` is repeatable: a drained channel never panics on `recv_opt`
//   5. `f64` payloads round-trip
//   6. `bool` payloads round-trip (true and false)
//   7. class (GC reference) payloads round-trip
//   8. a nested `Option<i64>` element keeps `Some(None)` distinct from `None`
//   9. `String` payloads survive GC stress
//  10. a receiver parked on an empty channel is woken by `close` with `None`
//  11. every parked receiver is woken by `close`
//  12. a bounded channel's blocked producer and a draining consumer finish
//  13. `recv_opt` as a call argument suspends inside an operand
//  14. select `let v = ch.recv_opt()` on a closed channel binds `None`
//  15. select `let v = ch.recv_opt()` on a ready channel binds `Some(v)`
//  16. an empty open channel falls through to `default`
//  17. select waits for a later `close` and then binds `None`
//  18. the discarded `ch.recv_opt() =>` form runs on close
//  19. a synchronous (non-async) select observes close
//  20. plain select `let v = ch.recv()` still raises on closed-empty
//  21. plain `recv()` on closed-empty still raises
//  22. `recv_opt(x)` is an arity error
//  23. `recv_opt` on a non-channel is rejected
//  24. the result is an `Option`, not the element type
//  25. a user class's own `recv_opt` method is still dispatched
//  26. `recv_opt` inside a lock body is rejected like `recv` (E2604)
//  27. `recv_opt(x)` inside select is rejected, not silently stripped
// ---------------------------------------------------------------------------

#[test]
fn close_aware_01_async_drain_until_closed() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i * 10);
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn drain(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    while true {
        match ch.recv_opt() {
            Some(v) => { println(v); total = total + v; }
            None => { break; }
        }
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    println(await drain(ch));
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n30\n60\n");
}

#[test]
fn close_aware_02_sync_drain_then_none() {
    let (out, ok) = compile_and_run(
        r#"
fn drain(ch: Channel<String>) -> i64 {
    let mut n = 0;
    while true {
        match ch.recv_opt() {
            Some(s) => { println(s); n = n + 1; }
            None => { break; }
        }
    }
    return n;
}
fn main() {
    let ch = Channel<String>::new();
    ch.send("a");
    ch.send("b");
    ch.close();
    println(drain(ch));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a\nb\n2\n");
}

#[test]
fn close_aware_03_buffered_values_precede_none_in_order() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(3);
    ch.send(1);
    ch.send(2);
    ch.close();
    println(match ch.recv_opt() { Some(v) => v, None => -1 });
    println(match ch.recv_opt() { Some(v) => v, None => -1 });
    println(match ch.recv_opt() { Some(v) => v, None => -1 });
    println(match ch.recv_opt() { Some(v) => v, None => -1 });
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n1\n2\n-1\n");
}

#[test]
fn close_aware_04_none_is_repeatable() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.close();
    let mut i = 0;
    while i < 3 {
        println(ch.recv_opt().is_none());
        i = i + 1;
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\ntrue\n");
}

#[test]
fn close_aware_05_f64_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<f64>::new();
    ch.send(2.5);
    ch.send(-0.125);
    ch.close();
    println(match ch.recv_opt() { Some(v) => v, None => 0.0 });
    println(match ch.recv_opt() { Some(v) => v, None => 0.0 });
    println(ch.recv_opt().is_some());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2.5\n-0.125\nfalse\n");
}

#[test]
fn close_aware_06_bool_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn show(v: Option<bool>) -> String {
    return match v { Some(b) => b.toString(), None => "closed" };
}
fn main() {
    let ch = Channel<bool>::new();
    ch.send(true);
    ch.send(false);
    ch.close();
    println(show(ch.recv_opt()));
    println(show(ch.recv_opt()));
    println(show(ch.recv_opt()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\nclosed\n");
}

#[test]
fn close_aware_07_class_payload() {
    let (out, ok) = compile_and_run(
        r#"
class Job {
    pub id: i64;
    pub init(self, id: i64) { self.id = id; }
}
async fn main() {
    let ch = Channel<Job>::new();
    ch.send(new Job(7));
    ch.send(new Job(9));
    ch.close();
    let mut sum = 0;
    while true {
        match ch.recv_opt() {
            Some(job) => { sum = sum + job.id; }
            None => { break; }
        }
    }
    println(sum);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n");
}

#[test]
fn close_aware_08_nested_option_element_stays_distinct() {
    let (out, ok) = compile_and_run(
        r#"
fn describe(v: Option<Option<i64>>) -> String {
    return match v {
        Some(inner) => match inner { Some(x) => x.toString(), None => "inner-none" },
        None => "closed",
    };
}
fn main() {
    let ch = Channel<Option<i64>>::new();
    ch.send(Some(4));
    ch.send(None);
    ch.close();
    println(describe(ch.recv_opt()));
    println(describe(ch.recv_opt()));
    println(describe(ch.recv_opt()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\ninner-none\nclosed\n");
}

#[test]
fn close_aware_09_string_payload_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    let mut i = 0;
    while i < 4 {
        await sleep(1);
        ch.send("item" + i.toString());
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    while true {
        match ch.recv_opt() {
            Some(s) => { println(s); }
            None => { break; }
        }
    }
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "item0\nitem1\nitem2\nitem3\n");
}

#[test]
fn close_aware_10_parked_receiver_woken_by_close() {
    let (out, ok) = compile_and_run(
        r#"
async fn consumer(ch: Channel<i64>) -> bool {
    return ch.recv_opt().is_none();
}
async fn main() {
    let ch = Channel<i64>::new();
    let c = consumer(ch);
    await sleep(10);
    ch.close();
    println(await c);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn close_aware_11_every_parked_receiver_woken_by_close() {
    let (out, ok) = compile_and_run(
        r#"
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut n = 0;
    while true {
        match ch.recv_opt() {
            Some(v) => { n = n + v; }
            None => { break; }
        }
    }
    return n;
}
async fn main() {
    let ch = Channel<i64>::new();
    let a = consumer(ch);
    let b = consumer(ch);
    let c = consumer(ch);
    await sleep(5);
    ch.send(1);
    ch.send(1);
    ch.send(1);
    await sleep(5);
    ch.close();
    println(await a + await b + await c);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn close_aware_12_bounded_channel_drain() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 5 {
        ch.send(i);
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::with_capacity(1);
    let p = producer(ch);
    let mut total = 0;
    while true {
        match ch.recv_opt() {
            Some(v) => { total = total + v; }
            None => { break; }
        }
    }
    await p;
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}

#[test]
fn close_aware_13_recv_opt_as_call_argument() {
    let (out, ok) = compile_and_run(
        r#"
fn or_zero(v: Option<i64>, bias: i64) -> i64 {
    return match v { Some(x) => x + bias, None => bias };
}
async fn feed(ch: Channel<i64>) -> i64 {
    await sleep(5);
    ch.send(40);
    await sleep(5);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let f = feed(ch);
    let bias = 2;
    println(or_zero(ch.recv_opt(), bias));
    println(or_zero(ch.recv_opt(), bias));
    await f;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n2\n");
}

#[test]
fn close_aware_14_select_binds_none_on_closed_channel() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.close();
    select {
        let v = ch.recv_opt() => { println(v.is_none()); }
        sleep(1000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn close_aware_15_select_binds_some_on_ready_channel() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<String>::new();
    ch.send("hello");
    ch.close();
    select {
        let v = ch.recv_opt() => {
            println(match v { Some(s) => s, None => "closed" });
        }
    }
    select {
        let v = ch.recv_opt() => {
            println(match v { Some(s) => s, None => "closed" });
        }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello\nclosed\n");
}

#[test]
fn close_aware_16_open_empty_channel_uses_default() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv_opt() => { println(v.is_some()); }
        default => { println("default"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "default\n");
}

#[test]
fn close_aware_17_select_waits_for_close() {
    let (out, ok) = compile_and_run(
        r#"
async fn closer(ch: Channel<i64>) -> i64 {
    await sleep(10);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let c = closer(ch);
    let mut seen = 0;
    while true {
        let mut done = false;
        select {
            let v = ch.recv_opt() => {
                match v {
                    Some(x) => { seen = seen + x; }
                    None => { done = true; }
                }
            }
        }
        if done {
            break;
        }
    }
    await c;
    println(seen);
    println("closed");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\nclosed\n");
}

#[test]
fn close_aware_18_discarded_select_form_runs_on_close() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.close();
    select {
        ch.recv_opt() => { println("closed-or-value"); }
        sleep(1000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "closed-or-value\n");
}

#[test]
fn close_aware_19_sync_select_observes_close() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn poll(ch: Channel<i64>) -> String {
    select {
        let v = ch.recv_opt() => {
            return match v { Some(x) => x.toString(), None => "closed" };
        }
        default => { return "empty"; }
    }
    return "unreachable";
}
fn main() {
    let ch = Channel<i64>::new();
    println(poll(ch));
    ch.send(5);
    println(poll(ch));
    ch.close();
    println(poll(ch));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "empty\n5\nclosed\n");
}

#[test]
fn close_aware_20_plain_select_recv_still_raises_on_closed() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.close();
    select {
        let v = ch.recv() => { println(v); }
        sleep(1000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("recv on closed empty channel"), "{out}");
}

#[test]
fn close_aware_21_plain_recv_still_raises_on_closed() {
    let (out, ok) = compile_and_run_check_exit(
        "fn main() { let ch = Channel<i64>::new(); ch.close(); println(ch.recv()); }\n",
    );
    assert!(!ok, "{out}");
    assert!(out.contains("recv on closed empty channel"), "{out}");
}

#[test]
fn close_aware_22_argument_is_an_arity_error() {
    assert_compile_error_contains(
        "fn main() { let ch = Channel<i64>::new(); let v = ch.recv_opt(1); }\n",
        &["error[E0201]", "recv_opt expects 0 arguments, got 1"],
    );
}

#[test]
fn close_aware_23_non_channel_receiver_is_rejected() {
    assert_compile_error_contains(
        "fn main() { let x = 5; let v = x.recv_opt(); }\n",
        &["error[E0806]", "cannot call `recv_opt` on `i64`"],
    );
}

#[test]
fn close_aware_24_result_is_an_option() {
    assert_compile_error_contains(
        "fn main() { let ch = Channel<i64>::new(); let v: i64 = ch.recv_opt(); }\n",
        &["Option<i64>"],
    );
}

#[test]
fn close_aware_25_user_class_method_is_dispatched() {
    let (out, ok) = compile_and_run(
        r#"
class Inbox {
    n: i64;
    pub init(self, n: i64) { self.n = n; }
    pub fn recv_opt(self) -> i64 { return self.n + 1; }
}
fn main() {
    let inbox = new Inbox(41);
    println(inbox.recv_opt());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn close_aware_26_recv_opt_in_lock_body_is_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let m = Mutex::new(0);
    let ch = Channel<i64>::new();
    lock m as value {
        let got = ch.recv_opt();
    }
}
"#,
        &["error[E2604]"],
    );
}

#[test]
fn close_aware_27_select_recv_opt_with_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv_opt(1) => { println(0); }
        default => { println(1); }
    }
}
"#,
        &["error[E0103]", "select `let` case must bind"],
    );
}
