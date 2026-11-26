use super::*;

// ── Select channel expressions evaluate exactly once ────────
// The channel expr used to be re-evaluated at probe, unregister, recv, AND
// on every wakeup re-probe — a side-effecting expression could register one
// channel and receive from another. Now: coop selects stash the pointer in a
// frame slot at entry; sync selects in a stack slot before the retry loop.

#[test]
fn seval_01_coop_select_single_eval_across_wakeup() {
    // pick() logs each evaluation. The select parks (empty channel), a later
    // send wakes it — the resume must NOT re-run pick(): exactly one "e".
    let (out, ok) = compile_and_run_with_test_sync(
        "class Src { pub ch: Channel<i64>; pub fn pick(self) -> Channel<i64> { println(\"e\"); return self.ch; } }\nasync fn worker(s: Src) -> i64 { let mut got = 0; select { let v = s.pick().recv() => { got = v; } } return got; }\nasync fn produce(ch: Channel<i64>, id: i64) { while !test_task_parked(id) { await yield(); } ch.send(5); }\nasync fn main() { let ch = Channel<i64>::new(); let s = new Src(ch); test_observe_next_task(); let w = worker(s); let p = produce(ch, test_observed_task()); println(await w); await p; }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "e\n5\n", "pick() must run exactly once");
}

#[test]
fn seval_02_sync_select_single_eval_across_retries() {
    // Sync select without default retries by driving the scheduler; the
    // channel expression must still be evaluated only once.
    let (out, ok) = compile_and_run_with_test_sync(
        "class Src { pub ch: Channel<i64>; pub fn pick(self) -> Channel<i64> { println(\"e\"); return self.ch; } }\nasync fn produce(ch: Channel<i64>, id: i64) { while test_channel_waits() <= id { await yield(); } ch.send(9); }\nfn exercise() { let ch = Channel<i64>::new(); let s = new Src(ch); test_observe_next_task(); let p = produce(ch, test_channel_waits()); let mut got = 0; select { let v = s.pick().recv() => { got = v; } } println(got); }\nasync fn main() { exercise(); }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "e\n9\n", "pick() must run exactly once across retries");
}

#[test]
fn sync_select_retry_uses_one_probe_graph_and_constant_idle_dispatch() {
    for cases in [1, 8, 64] {
        let declarations = (0..cases)
            .map(|i| format!("let c{i} = Channel<i64>::new();"))
            .collect::<String>();
        let arms = (0..cases)
            .map(|i| format!("let v = c{i}.recv() => {{ println(v); }}"))
            .collect::<String>();
        let source = format!("fn main() {{ {declarations} select {{ {arms} }} }}");
        let calls = compile_and_collect_relocation_targets_all(&source, &[]);
        for name in [
            "willow_select_drive_is_quiescent",
            "willow_select_idle_wait",
        ] {
            assert_eq!(
                calls.iter().filter(|call| call.as_str() == name).count(),
                1,
                "{cases} cases must share {name}"
            );
        }
        let probes = calls
            .iter()
            .filter(|call| call.as_str() == "willow_channel_recv_ready")
            .count();
        assert_eq!(
            probes, cases,
            "retry must reuse the existing case probe graph"
        );
    }
    for source in [
        "fn main() { let c = Channel<i64>::new(); select { let v = c.recv() => {} default => {} } }",
        "async fn main() { let c = Channel<i64>::new(); select { let v = c.recv() => {} } }",
    ] {
        let calls = compile_and_collect_relocation_targets_all(source, &[]);
        assert!(
            !calls
                .iter()
                .any(|call| call == "willow_select_drive_is_quiescent"),
            "default and cooperative selects must not acquire the synchronous slow path"
        );
    }
}

#[test]
fn sync_select_handoff_example_runs_with_one_and_many_workers() {
    for workers in ["1", "5"] {
        let (out, ok) = compile_and_run_with_env(
            include_str!("../../../example/select_sync_handoff.wi"),
            &[("WILLOW_WORKERS", workers)],
        );
        assert!(ok, "{out}");
        assert_eq!(out, "42\n");
    }
}
