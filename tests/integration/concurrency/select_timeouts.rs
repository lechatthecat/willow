use super::*;

// Nonwinning 60s timeout arms are hang guards: the existing expected-output
// assertions fail if one fires. Readiness/order comes from explicit state;
// timer-order regressions use a clock advanced only by the test driver.

// ── select timeout + task-completion cases ────────────────────
// `sleep(ms) =>` : deadline fixed ONCE at select entry; parks with the
// scheduler timer armed to the nearest deadline; ready when now >= deadline.
// `let v = await t =>` registers on the task's waiter list (via
// willow_sched_await), unregisters when another case wins, and reads the
// result with await semantics (cancelled -> located panic).
// 20 perspectives: 1 coop timeout fires on empty channel, 2 coop recv wins
// before timeout, 3 sync timeout fires, 4 sync recv wins, 5 coop task await
// wins before timeout, 6 sync task-await case is rejected, 7 await binding is
// typed as the task result, 8 discard await binding, 9 timeout+default wins,
// 10 two timeouts: nearer fires, 11 await of an already-completed task is
// immediate, 12 await of a CANCELLED task panics, 13 send
// case still wins over timeout, 14 timeout body can suspend (coop),
// 15 deadline fixed at entry (late wakeups don't extend it), 16 timer
// select in a LOOP re-arms each iteration, 17 GC stress with timeout +
// string channel, 18 task-await case with String result, 19 other-case win
// unregisters the task waiter (no spurious wake corruption; program exits
// clean), 20 checker rejects non-i64 sleep arg.

#[test]
fn stmo_01_coop_timeout_fires() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(30) => { println(\"t\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "t\n");
}

#[test]
fn stmo_02_coop_recv_beats_timeout() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn feed(ch: Channel<i64>, owner: i64) { while !test_task_parked(owner) { await yield(); } ch.send(5); }\nasync fn main() { let ch = Channel<i64>::new(); let f = feed(ch, test_current_task()); select { let v = ch.recv() => { println(v); } sleep(60000) => { println(\"t\"); } } await f; }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn stmo_04_sync_recv_beats_timeout() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); ch.send(4); select { let v = ch.recv() => { println(v); } sleep(60000) => { println(\"t\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn stmo_05_coop_task_await_beats_timeout() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn quick(owner: i64) -> i64 { while !test_task_parked(owner) { await yield(); } return 7; }\nasync fn main() { let t = quick(test_current_task()); select { let v = await t => { println(v); } sleep(60000) => { println(\"late\"); } } }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn stmo_06_sync_task_case_is_rejected() {
    // A task case IS an `await`, so a sync `select` cannot wait on a task; its channel and
    // `sleep` cases stay legal.
    let (ok, stderr) = compile_with_compiler_env(
        "async fn quick() -> i64 { await sleep(10); return 9; }\nfn main() { let t = quick(); select { let v = await t => { println(v); } sleep(60000) => { println(\"late\"); } } }",
        &[],
    );
    assert!(!ok);
    for expected in [
        "error[E0801]",
        "`await` can only be used inside an async function",
    ] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
}

#[test]
fn stmo_06b_sync_select_still_allows_channel_and_sleep_cases() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(5) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "late\n");
}

#[test]
fn stmo_07_await_binding_typed() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn quick(owner: i64) -> i64 { while !test_task_parked(owner) { await yield(); } return 20; }\nasync fn main() { let t = quick(test_current_task()); select { let v = await t => { println(v + 1); } sleep(60000) => { } } }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "21\n");
}

#[test]
fn stmo_08_discard_await_binding() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn quick(owner: i64) { while !test_task_parked(owner) { await yield(); } }\nasync fn main() { let t = quick(test_current_task()); select { await t => { println(8); } sleep(60000) => { } } }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn stmo_09_default_beats_timeout() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(60000) => { println(\"t\"); } default => { println(\"d\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "d\n");
}

#[test]
fn stmo_10_nearer_timeout_fires() {
    // Both deadlines are created at virtual time zero. Advance only after
    // registration, so host preemption cannot make the far arm ready too.
    let (out, ok) = compile_and_run_with_virtual_time(
        r#"
async fn advance() {
    while test_channel_waits() < 1 { await yield(); }
    test_set_time(30);
}
fn exercise() {
    let ch = Channel<i64>::new();
    advance();
    select {
        let v = ch.recv() => { println(v); }
        sleep(5000) => { println("far"); }
        sleep(30) => { println("near"); }
    }
}
async fn main() { test_set_time(0); test_observe_next_task(); exercise(); }
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "near\n");
}

#[test]
fn stmo_11_completed_task_immediate() {
    let (out, ok) = compile_and_run(
        "async fn quick() -> i64 { return 3; }\nasync fn main() { let t = quick(); await t; select { let v = await t => { println(v); } sleep(60000) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn stmo_12_cancelled_await_panics() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn slow() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nasync fn main() { let t = slow(); t.cancel(); await t.result(); select { let v = await t => { println(v); } sleep(60000) => { } } }",
    );
    assert!(!ok);
    assert!(out.contains("cancelled task"), "{out}");
}

#[test]
fn stmo_13_send_beats_timeout() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { ch.send(1) => { println(\"sent\"); } sleep(60000) => { println(\"t\"); } } println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n1\n");
}

#[test]
fn stmo_14_timeout_body_suspends() {
    let (out, ok) = compile_and_run(
        "async fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(20) => { await yield(); println(\"after\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "after\n");
}

#[test]
fn stmo_15_deadline_fixed_at_entry() {
    // A spurious wake forces a second registration at time 10. The original
    // time-30 deadline must survive it; restarting at 10 would wait forever
    // after the driver stops the virtual clock at 30.
    let (out, ok) = compile_and_run_with_virtual_time(
        r#"
fn delay(calls: AtomicI64) -> i64 { calls.add(1); return 30; }
async fn poke(ch: Channel<i64>, gate: Channel<i64>, owner: i64) {
    while test_task_parks(owner) < 1 { await yield(); }
    test_set_time(10);
    test_wake_task(owner);
    while test_task_parks(owner) < 2 { await yield(); }
    test_set_time(30);
    test_wake_task(owner);
    gate.recv();
    ch.send(1);
}
async fn main() {
    test_set_time(0);
    test_observe_next_task();
    let ch = Channel<i64>::new();
    let gate = Channel<i64>::new();
    let calls = AtomicI64::new(0);
    let p = poke(ch, gate, test_current_task());
    select {
        let v = ch.recv() => { println(v); }
        sleep(delay(calls)) => { println("t"); }
    }
    if calls.load() != 1 { panic("deadline expression was evaluated again"); }
    gate.send(1);
    await p;
    if ch.recv() != 1 { panic("late send was lost"); }
}
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "t\n");
}

#[test]
fn stmo_16_loop_rearms() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); let mut n = 0; while n < 3 { select { let v = ch.recv() => { println(v); } sleep(15) => { n = n + 1; } } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn stmo_17_gc_stress_timeout_string_channel() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { let ch = Channel<String>::new(); select { let v = ch.recv() => { println(v); } sleep(30) => { println(\"g\" + \"c\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "gc\n");
}

#[test]
fn stmo_18_await_string_result() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn name(owner: i64) -> String { while !test_task_parked(owner) { await yield(); } return \"wil\" + \"low\"; }\nasync fn main() { let t = name(test_current_task()); select { let v = await t => { println(v); } sleep(60000) => { } } }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

#[test]
fn stmo_19_other_win_unregisters_task_waiter() {
    // Both sources are pending when select registers. Release only the channel,
    // then complete the losing task after the select has removed its waiter.
    let (out, ok) = compile_and_run_with_test_sync(
        r#"
async fn slow(gate: Channel<i64>) -> i64 { gate.recv(); return 2; }
async fn feed(ch: Channel<i64>, owner: i64) {
    while !test_task_parked(owner) { await yield(); }
    ch.send(1);
}
async fn main() {
    let ch = Channel<i64>::new();
    let gate = Channel<i64>::new();
    let t = slow(gate);
    let f = feed(ch, test_current_task());
    select {
        let v = ch.recv() => { println(v); }
        let w = await t => { println(w); }
    }
    gate.send(1);
    await t;
    await f;
    println(9);
}
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n9\n");
}

#[test]
fn stmo_20_non_i64_sleep_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { } sleep(\"x\") => { } } }",
        &[],
    );
    assert!(!ok);
    assert!(!stderr.is_empty());
}
