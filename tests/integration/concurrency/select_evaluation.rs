use super::*;

// ── Select channel expressions evaluate exactly once (willow-0a6k.6) ────────
// The channel expr used to be re-evaluated at probe, unregister, recv, AND
// on every wakeup re-probe — a side-effecting expression could register one
// channel and receive from another. Now: coop selects stash the pointer in a
// frame slot at entry; sync selects in a stack slot before the retry loop.

#[test]
fn seval_01_coop_select_single_eval_across_wakeup() {
    // pick() logs each evaluation. The select parks (empty channel), a later
    // send wakes it — the resume must NOT re-run pick(): exactly one "e".
    let (out, ok) = compile_and_run(
        "class Src { pub ch: Channel<i64>; pub fn pick(self) -> Channel<i64> { println(\"e\"); return self.ch; } }\nasync fn worker(s: Src) -> i64 { let mut got = 0; select { let v = s.pick().recv() => { got = v; } } return got; }\nasync fn produce(ch: Channel<i64>) { await sleep(30); ch.send(5); }\nasync fn main() { let ch = Channel<i64>::new(); let s = new Src(ch); let w = worker(s); let p = produce(ch); println(await w); await p; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "e\n5\n", "pick() must run exactly once");
}

#[test]
fn seval_02_sync_select_single_eval_across_retries() {
    // Sync select without default retries by driving the scheduler; the
    // channel expression must still be evaluated only once.
    let (out, ok) = compile_and_run(
        "class Src { pub ch: Channel<i64>; pub fn pick(self) -> Channel<i64> { println(\"e\"); return self.ch; } }\nasync fn produce(ch: Channel<i64>) { await sleep(30); ch.send(9); }\nfn main() { let ch = Channel<i64>::new(); let s = new Src(ch); let p = produce(ch); let mut got = 0; select { let v = s.pick().recv() => { got = v; } } println(got); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "e\n9\n", "pick() must run exactly once across retries");
}
