use super::*;

// ── Preempt 2 closure: safepoint coverage decisions pinned (willow-0a6k.2) ──
// The task-aware design closed as: (1) coop loops (ALL while/for in async
// fns, await or not) carry backedge safepoints; (2) long sync helpers called
// from task context are REJECTED (E0810) rather than safepoint-cloned;
// (3) fn-values/lambdas cannot smuggle a nonpreemptible helper into a task —
// the Send rule forbids fn values in async frames (E2402); (4) statement
// call-bearing statement boundaries carry safepoints; (5) runtime calls
// (string/array/map ops) are
// BOUNDED no-preempt regions and run to completion. 11 pinning perspectives
// (psp_07b added by Stage A-prime, willow-38w.2.1).

#[test]
fn psp_01_await_free_while_is_preemptible() {
    // Busy while-true task must not starve the sleeping task (tiny quantum).
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin() -> i64 { let mut i = 0; while true { i = i + 1; } return i; }\nasync fn main() { let t = spin(); await sleep(30); println(1); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn psp_02_await_free_for_is_preemptible() {
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin() -> i64 { let mut s = 0; for i in 0..1000000000 { s = s + i; } return s; }\nasync fn main() { let t = spin(); await sleep(30); println(2); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn psp_03_nested_busy_loops_preemptible() {
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin() -> i64 { let mut s = 0; while true { for i in 0..1000 { s = s + i; } } return s; }\nasync fn main() { let t = spin(); await sleep(30); println(3); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn psp_04_fn_value_cannot_enter_task() {
    // A lambda calling a nonpreemptible helper can never ride into a task:
    // fn values are not Send (closes the E0810 dataflow gap structurally).
    let (ok, stderr) = compile_with_compiler_env(
        "fn heavy() -> i64 { let mut i = 0; while true { i = i + 1; } return i; }\nasync fn worker() -> i64 { let f = || heavy(); return f(); }\nasync fn main() { println(await worker()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E2402"), "{stderr}");
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn psp_05_sync_helper_loop_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn heavy() -> i64 { let mut i = 0; while true { i = i + 1; } return i; }\nasync fn worker() -> i64 { return heavy(); }\nasync fn main() { println(await worker()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0810"), "{stderr}");
}

#[test]
fn psp_06_busy_tasks_exceed_worker_count() {
    // SIX busy spinners on five workers (review fix: three spinners never
    // exhausted the pool): the sleeper still runs, so preemption must be
    // rotating workers off busy tasks, not just parking spare workers.
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin() -> i64 { let mut i = 0; while true { i = i + 1; } return i; }\nasync fn main() { let a = spin(); let b = spin(); let c = spin(); let d = spin(); let e = spin(); let f = spin(); await sleep(50); println(6); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(25),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn psp_07_call_boundary_safepoints() {
    // Call-boundary safepoints, isolated from loop backedges (review
    // fix: a for-loop body only proved the BACKEDGE safepoint): the spinning
    // task is a straight-line chain of call-bearing statements — bounded sync
    // helper calls with no Willow loop between them at the top level — so only
    // the safepoints before those call statements can let the sleeper run
    // before the chain finishes.
    //
    // The helper is bounded: loop-free AND non-recursive. It used to recurse
    // (`chunk(n - 1) + chunk(n - 2)`), which made every statement
    // multi-millisecond, but Stage A-prime rejects a recursive sync helper
    // called from task context because recursion runs unbounded work with no
    // safepoint (E0810, willow-38w.2.1) — `psp_07b` pins that rejection. The
    // timing-sensitive form returns as a Stage G acceptance case once
    // task-aware sync-stack preemption ships.
    let chunk = "s = s + chunk(s);";
    let chain = chunk.repeat(50);
    let source = format!(
        "fn chunk(n: i64) -> i64 {{ if n <= 1 {{ return 1; }} return n / 2 + 1; }}\nasync fn churn() -> i64 {{ let mut s = 0; {chain} return s; }}\nasync fn main() {{ let t = churn(); await sleep(10); println(7); await t; }}"
    );
    let (out, ok) = compile_and_run_with_runtime_env(
        &source,
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn psp_07b_recursive_sync_helper_rejected() {
    // Stage A-prime (willow-38w.2.1): recursion contains no loop but still runs
    // unbounded work on a scheduler worker, so a task-context call to it is
    // rejected. The message must name recursion rather than claim a loop the
    // programmer would then go looking for.
    let (ok, stderr) = compile_with_compiler_env(
        "fn chunk(n: i64) -> i64 { if n <= 1 { return n; } return chunk(n - 1) + chunk(n - 2); }\nasync fn churn() -> i64 { return chunk(24); }\nasync fn main() { println(await churn()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0810"), "{stderr}");
    assert!(
        stderr.contains("sync helper `chunk` can run unbounded recursive work in task context"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("with a loop"),
        "recursion must not be reported as a loop:\n{stderr}"
    );
}

#[test]
fn psp_08_gc_stress_with_busy_loop() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn churn() -> String { let mut s = \"\"; for i in 0..50 { s = s + \"x\"; } return s; }\nasync fn main() { let t = churn(); await sleep(20); println(await t == \"\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn psp_09_bounded_runtime_call_completes() {
    // Runtime helpers (array ops) are bounded no-preempt regions: they finish
    // and the task remains schedulable around them.
    let (out, ok) = compile_and_run_with_runtime_env(
        "import std::collections::Array;\nasync fn build() -> i64 { let xs: Array<i64> = []; let mut i = 0; while i < 100000 { xs.push(i); i = i + 1; } return xs.len(); }\nasync fn main() { println(await build()); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(25),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100000\n");
}

#[test]
fn psp_10_cancel_preempted_busy_task() {
    // Preemption + cancellation compose: an await-free busy task is
    // preempted at a backedge safepoint and finalized by cancel.
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin() -> i64 { let mut i = 0; while true { i = i + 1; } return i; }\nasync fn main() { let t = spin(); await sleep(30); t.cancel(); await sleep(50); println(t.is_cancelled()); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5")],
        std::time::Duration::from_secs(25),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}
