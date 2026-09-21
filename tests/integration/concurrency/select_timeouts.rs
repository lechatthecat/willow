use super::*;

// ── select timeout + task-completion cases (willow-soro) ────────────────────
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
    let (out, ok) = compile_and_run(
        "async fn feed(ch: Channel<i64>) { await sleep(10); ch.send(5); }\nasync fn main() { let ch = Channel<i64>::new(); let f = feed(ch); select { let v = ch.recv() => { println(v); } sleep(5000) => { println(\"t\"); } } await f; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn stmo_03_sync_timeout_fires() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(30) => { println(\"t\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "t\n");
}

#[test]
fn stmo_04_sync_recv_beats_timeout() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); ch.send(4); select { let v = ch.recv() => { println(v); } sleep(5000) => { println(\"t\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn stmo_05_coop_task_await_beats_timeout() {
    let (out, ok) = compile_and_run(
        "async fn quick() -> i64 { await sleep(10); return 7; }\nasync fn main() { let t = quick(); select { let v = await t => { println(v); } sleep(5000) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn stmo_06_sync_task_case_is_rejected() {
    // A task case IS an `await`, so a sync `select` cannot wait on a task
    // (willow-qrj9); its channel and `sleep` cases stay legal.
    let (ok, stderr) = compile_with_compiler_env(
        "async fn quick() -> i64 { await sleep(10); return 9; }\nfn main() { let t = quick(); select { let v = await t => { println(v); } sleep(5000) => { println(\"late\"); } } }",
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
    let (out, ok) = compile_and_run(
        "async fn quick() -> i64 { await sleep(5); return 20; }\nasync fn main() { let t = quick(); select { let v = await t => { println(v + 1); } sleep(5000) => { } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "21\n");
}

#[test]
fn stmo_08_discard_await_binding() {
    let (out, ok) = compile_and_run(
        "async fn quick() { await sleep(5); }\nasync fn main() { let t = quick(); select { await t => { println(8); } sleep(5000) => { } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn stmo_09_default_beats_timeout() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(5000) => { println(\"t\"); } default => { println(\"d\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "d\n");
}

#[test]
fn stmo_10_nearer_timeout_fires() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(5000) => { println(\"far\"); } sleep(30) => { println(\"near\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "near\n");
}

#[test]
fn stmo_11_completed_task_immediate() {
    let (out, ok) = compile_and_run(
        "async fn quick() -> i64 { return 3; }\nasync fn main() { let t = quick(); await t; select { let v = await t => { println(v); } sleep(5000) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn stmo_12_cancelled_await_panics() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn slow() -> i64 { await sleep(5000); return 1; }\nasync fn main() { let t = slow(); await sleep(20); t.cancel(); await sleep(30); select { let v = await t => { println(v); } sleep(5000) => { } } }",
    );
    assert!(!ok);
    assert!(out.contains("cancelled task"), "{out}");
}

#[test]
fn stmo_13_send_beats_timeout() {
    let (out, ok) = compile_and_run(
        "fn main() { let ch = Channel<i64>::new(); select { ch.send(1) => { println(\"sent\"); } sleep(5000) => { println(\"t\"); } } println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n1\n");
}

#[test]
fn stmo_14_timeout_body_suspends() {
    let (out, ok) = compile_and_run(
        "async fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(20) => { await sleep(10); println(\"after\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "after\n");
}

#[test]
fn stmo_15_deadline_fixed_at_entry() {
    // A send wakes the select mid-wait but the recv drains to a LOSING value
    // only after the deadline: re-probes must keep the ORIGINAL deadline.
    let (out, ok) = compile_and_run(
        "async fn poke(ch: Channel<i64>) { await sleep(60); ch.send(1); }\nasync fn main() { let ch = Channel<i64>::new(); let p = poke(ch); select { let v = ch.recv() => { println(v); } sleep(30) => { println(\"t\"); } } await p; }",
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
    let (out, ok) = compile_and_run(
        "async fn name() -> String { await sleep(10); return \"wil\" + \"low\"; }\nasync fn main() { let t = name(); select { let v = await t => { println(v); } sleep(5000) => { } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

#[test]
fn stmo_19_other_win_unregisters_task_waiter() {
    // recv wins; the task-wait registration must be removed — the later task
    // completion must not corrupt/wake the finished select (clean exit).
    let (out, ok) = compile_and_run(
        "async fn slow() -> i64 { await sleep(60); return 2; }\nasync fn main() { let ch = Channel<i64>::new(); ch.send(1); let t = slow(); select { let v = ch.recv() => { println(v); } let w = await t => { println(w); } } await sleep(100); println(9); }",
    );
    assert!(ok, "{out}");
    assert!(out.ends_with("9\n"), "{out}");
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
