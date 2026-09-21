use super::*;

// ── Happens-before guarantees + Channel item Send (willow-dgwo.6) ────────────
// Perspectives: 1 channel send->recv value visible; 2 channel order preserved;
// 3 Mutex counter no lost updates; 4 AtomicI64 counter no lost updates; 5 await
// makes a task's result visible; 6 Channel<Fn> send rejected under the check
// (E2403); 7 low worker overrides still reject it; 8 Channel<i64> is accepted.
#[test]
fn test_dgwo6_channel_send_recv_value_and_order() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut x = 0;
    x = 41;
    x = x + 1;        // write happens-before the send
    ch.send(x);
    ch.send(100);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    println(ch.recv());   // 42 — the pre-send write is visible
    println(ch.recv());   // 100 — order preserved
    await p;
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n100\n");
}

#[test]
fn test_dgwo6_mutex_value_visible_across_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn store(m: Mutex<i64>, value: i64) -> i64 {
    await sleep(1);
    lock m as mut cell {
        cell = value;
    }
    return value;
}
async fn main() {
    let m = Mutex::new(0);
    let writer = store(m, 10);
    await writer;
    lock m as cell {
        println(cell);   // the awaited task's write is visible
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_dgwo6_atomic_counter_no_lost_updates() {
    let (out, ok) = compile_and_run(
        r#"
async fn inc(c: AtomicI64, n: i64) -> i64 {
    let mut i = 0;
    while i < n { c.add(1); await sleep(1); i = i + 1; }
    return n;
}
async fn main() {
    let c = AtomicI64::new(0);
    let a = inc(c, 5);
    let b = inc(c, 7);
    await a;
    await b;
    println(c.load());   // 12
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_dgwo6_await_makes_task_result_visible() {
    let (out, ok) = compile_and_run(
        r#"
async fn compute() -> i64 { await sleep(1); return 7 * 6; }
async fn main() {
    let t = compute();
    println(await t);   // 42 — the task's writes are visible after await
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_dgwo6_channel_item_must_be_send_e2403() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
fn dbl(x: i64) -> i64 { return x * 2; }
fn main() {
    let ch = Channel<fn(i64) -> i64>::new();
    let f = dbl;
    ch.send(f);
}
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2403]"), "{stderr}");
    assert!(stderr.contains("must be `Send`"), "{stderr}");
}

#[test]
fn test_dgwo6_channel_fn_send_rejected_by_default() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
fn dbl(x: i64) -> i64 { return x * 2; }
fn main() {
    let ch = Channel<fn(i64) -> i64>::new();
    ch.send(dbl);
    let g = ch.recv();
    println(g(21));   // 42
}
"#,
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2403]"), "{stderr}");
}

#[test]
fn test_dgwo6_channel_send_send_value_ok_under_check() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(5);
    println(ch.recv());
}
"#,
    );
    assert!(ok, "Channel<i64> send should be accepted: {stderr}");
}

#[test]
fn test_dgwo4_scalar_only_and_nested_forwarding_accepted() {
    // 19 + 20: scalar-only async fn, and a nested async call forwarding a Sync
    // argument, are both accepted.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
async fn inner(m: Mutex<i64>) -> i64 { await sleep(1); lock m as cell { return cell; } }
async fn outer(m: Mutex<i64>, n: i64) -> i64 { return await inner(m) + n; }
async fn main() {
    let m = Mutex::new(5);
    println(await outer(m, 1));
}
"#,
    );
    assert!(
        ok,
        "scalar + nested Sync forwarding should be accepted: {stderr}"
    );
}
