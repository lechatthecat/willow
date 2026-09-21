use super::*;

// ----------------------------------------------------------------------------
// select (willow-7aj): wait on multiple channel ops. A recv case is ready when
// its channel has a value or is closed; a send case (unbounded) is always
// ready; the first ready case runs; `default` runs when nothing is ready.
// ----------------------------------------------------------------------------

#[test]
fn select_01_default_on_empty() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn select_02_recv_ready_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.send(42);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn select_03_recv_drives_scheduler_until_producer() {
    // No default: select drives the scheduler until a spawned producer sends.
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(99);
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn select_04_first_ready_of_multiple_recv() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    b.send(7);
    select {
        let x = a.recv() => { println(x + 1000); }
        let y = b.recv() => { println(y); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn select_05_send_case() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let out = Channel<i64>::new();
    select {
        out.send(55) => { println(1); }
        default => { println(-1); }
    }
    println(out.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n55\n");
}

#[test]
fn select_06_string_channel_literal_gc() {
    // A String channel select-send of a literal queues correctly (literal must
    // be collected from the select case), and survives GC stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let ch = Channel<String>::new();
    select {
        ch.send("hello") => { println(1); }
    }
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\nhello\n");
}

#[test]
fn select_07_non_channel_is_error() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let x = 5;
    select {
        let v = x.recv() => { println(v); }
    }
}
"#,
        &["error[E0807]", "Channel"],
    );
}

// willow-lpn.7: a task parked on a TIMER keeps its async-frame GC roots alive
// while a CONCURRENT task triggers collection. The sleeper's frame is a runtime
// root while parked, so its live String survives.
#[test]
fn coop_gc_06_timer_parked_frame_survives_concurrent_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sleeper() -> i64 {
    let s = "kept-across-timer-park";
    await sleep(5);
    println(s);
    return 0;
}
async fn collector() -> i64 {
    await sleep(1);
    gc_collect();
    let junk = "x" + "y";
    gc_collect();
    return 0;
}
async fn main() {
    let a = sleeper();
    let b = collector();
    await a;
    await b;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept-across-timer-park\n");
}

// ── willow-7aj: cooperative-suspend `select` (a select INSIDE a task PARKS on
// its channels instead of block-driving). 20 test perspectives:
//  1. single recv parks when empty, woken by a later send -> receives value
//  2. repeated select in a while loop (park/wake each iteration)
//  3. multi-channel select: parks on all, woken by whichever is ready first
//  4. multi-channel across iterations (channel a then channel b)
//  5. default present + channel empty -> default branch runs (no park)
//  6. default present + channel ready -> ready branch runs (default skipped)
//  7. send case is always ready and fires
//  8. Channel<String> recv binding is GC-traced (survives gc_collect after recv)
//  9. recv binding is usable inside the case body
// 10. case body with its OWN suspend (await sleep) after the binding -> binding survives
// 11. send followed by close wakes a parked select and drains the buffered value
// 12. unregister: after picking channel a, a later send on the OTHER channel b
//     does not corrupt the next select iteration
// 13. `_` discard binding recv
// 14. select nested in a while loop summing values (canonical consumer)
// 15. source-order priority when multiple recv cases are ready
// 16. send-case value matches the channel element type
// 17. a select-only task is a cooperative leaf (task await works)
// 18. whole thing under WILLOW_GC_STRESS=all
// 19. select runs in a spawned task awaited by main
// 20. case body contains a second recv (nested suspend points)

#[test]
fn coop_select_01_single_recv_parks_and_wakes() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(42); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    select { let v = ch.recv() => { total = v; } }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c); await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_select_02_while_loop_sum() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1); ch.send(10);
    await sleep(1); ch.send(20);
    await sleep(1); ch.send(30);
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        select { let v = ch.recv() => { total = total + v; } }
        i = i + 1;
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c); await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "60\n");
}

#[test]
fn coop_select_03_multi_channel_parks_on_both() {
    // Perspectives 3, 4, 12: parks on both channels; after a wakes it, the next
    // iteration parks again and b wakes it; unregistering from the non-chosen
    // channel keeps the second iteration correct.
    let (out, ok) = compile_and_run(
        r#"
async fn p1(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(100); return 0; }
async fn p2(ch: Channel<i64>) -> i64 { await sleep(2); ch.send(200); return 0; }
async fn consumer(a: Channel<i64>, b: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut n = 0;
    while n < 2 {
        select {
            let v = a.recv() => { total = total + v; }
            let v = b.recv() => { total = total + v; }
        }
        n = n + 1;
    }
    return total;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let x = p1(a);
    let y = p2(b);
    let c = consumer(a, b);
    println(await c); await x; await y;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "300\n");
}

#[test]
fn coop_select_04_default_when_empty() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(ch: Channel<i64>) -> i64 {
    await sleep(1);
    let mut hit = 0;
    select {
        let v = ch.recv() => { hit = v; }
        default => { hit = -1; }
    }
    return hit;
}
async fn main() {
    let ch = Channel<i64>::new();
    let w = worker(ch);
    println(await w);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn coop_select_05_default_skipped_when_ready() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(ch: Channel<i64>) -> i64 {
    ch.send(5);
    await sleep(1);
    let mut hit = 0;
    select {
        let v = ch.recv() => { hit = v; }
        default => { hit = -1; }
    }
    return hit;
}
async fn main() {
    let ch = Channel<i64>::new();
    let w = worker(ch);
    println(await w);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn coop_select_06_send_case() {
    let (out, ok) = compile_and_run(
        r#"
async fn sender(ch: Channel<i64>) -> i64 {
    await sleep(1);
    select { ch.send(7) => { } }
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 { let v = ch.recv(); return v; }
async fn main() {
    let ch = Channel<i64>::new();
    let s = sender(ch);
    let c = consumer(ch);
    println(await c); await s;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn coop_select_07_string_binding_gc_safe() {
    // Perspectives 8, 18: the recv binding's frame slot is GC-traced.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    let s = "hello-" + "world";
    ch.send(s);
    return 0;
}
async fn consumer(ch: Channel<String>) -> i64 {
    let mut out = "empty";
    select { let v = ch.recv() => { out = v; } }
    gc_collect();
    println(out);
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch);
    await c; await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello-world\n");
}

#[test]
fn coop_select_08_send_then_close_wakes_and_drains_value() {
    // Perspective 11: close after a send wakes a parked select; recv drains the
    // buffered value instead of observing closed-empty.
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(0); ch.close(); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut got = 99;
    select { let v = ch.recv() => { got = v; } }
    return got;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c); await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn coop_select_09_case_body_nested_suspend() {
    // Perspectives 10, 20: the case body itself suspends (await sleep, then a
    // second recv) after binding; the binding and locals survive those suspends.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1); ch.send(11);
    await sleep(1); ch.send(22);
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    select {
        let v = ch.recv() => {
            await sleep(1);
            let w = ch.recv();
            total = v + w;
        }
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c); await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "33\n");
}

#[test]
fn coop_select_10_fair_pick_among_ready() {
    // Perspectives 13, 15 (revised, willow-0a6k.6): when several recv cases
    // are ready the pick is PSEUDO-RANDOMIZED (mixed global counter) rather
    // than always favoring source order — across many one-shot selects BOTH
    // cases must win at least once. This checks absence of systematic
    // source-order starvation, not bounded fairness. `_` discard binding is
    // allowed; each iteration drains both channels so readiness is identical
    // every time.
    let (out, ok) = compile_and_run(
        r#"
async fn round(a: Channel<i64>, b: Channel<i64>) -> i64 {
    a.send(1);
    b.send(2);
    await sleep(1);
    let mut picked = 0;
    select {
        let _ = a.recv() => { picked = 10; }
        let v = b.recv() => { picked = v; }
    }
    // Drain whichever value the losing case left behind.
    select {
        let _ = a.recv() => { }
        let _ = b.recv() => { }
        default => { }
    }
    return picked;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let mut saw_first = false;
    let mut saw_second = false;
    let mut i = 0;
    while i < 20 {
        let w = round(a, b);
        let picked = await w;
        if picked == 10 { saw_first = true; }
        if picked == 2 { saw_second = true; }
        i = i + 1;
    }
    println(saw_first);
    println(saw_second);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}
