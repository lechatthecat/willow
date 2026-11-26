use super::*;

// ── Preempt 2 closure: safepoint coverage decisions pinned ──
// The task-aware design closed as: (1) coop loops (ALL while/for in async
// fns, await or not) carry backedge safepoints; (2) long sync helpers called
// from task context are REJECTED (E0810) rather than safepoint-cloned;
// (3) shareable function values can enter tasks; native sync-stack
// safepoints keep their callees preemptible; (4) statement
// call-bearing statement boundaries carry safepoints; (5) runtime calls
// (string/array/map ops) are
// BOUNDED no-preempt regions and run to completion. 11 pinning perspectives
// (psp_07b added by Stage A-prime, stack safepoints).

#[test]
fn psp_01_await_free_while_is_preemptible() {
    // One worker must preempt the entered busy loop so main can cancel it.
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin(started: AtomicBool) -> i64 { let mut i = 0; while true { i = i + 1; started.store(true); } return i; }\nasync fn main() { let started = AtomicBool::new(false); let t = spin(started); while !started.load() { await yield(); } t.cancel(); await t.result(); println(1); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5"), ("WILLOW_WORKERS", "1")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn psp_02_await_free_for_is_preemptible() {
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin(started: AtomicBool, done: AtomicBool) -> i64 { let mut s = 0; for i in 0..9223372036854775807 { started.store(true); if done.load() { return s; } s = i; } panic(\"for loop exhausted before release\"); return s; }\nasync fn main() { let started = AtomicBool::new(false); let done = AtomicBool::new(false); let t = spin(started, done); while !started.load() { await yield(); } done.store(true); await t; println(2); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5"), ("WILLOW_WORKERS", "1")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn psp_03_nested_busy_loops_preemptible() {
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin(started: AtomicBool) -> i64 { let mut s = 0; while true { for i in 0..1000 { started.store(true); s = i; } } return s; }\nasync fn main() { let started = AtomicBool::new(false); let t = spin(started); while !started.load() { await yield(); } t.cancel(); await t.result(); println(3); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5"), ("WILLOW_WORKERS", "1")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[cfg(any(
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
))]
#[test]
fn psp_04_fn_value_can_enter_task() {
    // A pointer to a looping sync helper must still yield on native task stacks.
    let (out, ok) = compile_and_run_with_runtime_env(
        "fn spin(done: AtomicBool, started: AtomicBool) -> i64 { while !done.load() { started.store(true); } return 42; }\nasync fn worker(f: fn(AtomicBool, AtomicBool) -> i64, done: AtomicBool, started: AtomicBool) -> i64 { return f(done, started); }\nasync fn main() { let done = AtomicBool::new(false); let started = AtomicBool::new(false); let t = worker(spin, done, started); while !started.load() { await yield(); } done.store(true); println(await t); }",
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")],
        std::time::Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
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
    // All six tasks must enter while five workers remain occupied until release.
    let (out, ok) = compile_and_run_with_runtime_env(
        "async fn spin(started: AtomicI64, done: AtomicBool) { started.add(1); while !done.load() {} }\nasync fn main() { let started = AtomicI64::new(0); let done = AtomicBool::new(false); let a = spin(started, done); let b = spin(started, done); let c = spin(started, done); let d = spin(started, done); let e = spin(started, done); let f = spin(started, done); while started.load() < 6 { await yield(); } done.store(true); await a; await b; await c; await d; await e; await f; println(6); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(25),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[cfg(any(
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
))]
#[test]
fn psp_07_call_boundary_safepoints() {
    // The recursion remains bounded and loop-free. Its leaf publishes inside=true;
    // the synchronous wrapper clears it before outer driver loop safepoints.
    // One worker can therefore observe true only after a sync call-boundary yield.
    let source = "fn chunk(n: i64, inside: AtomicBool, release: AtomicBool) -> i64 { if release.load() { return 0; } if n <= 1 { inside.store(true); return 1; } return chunk(n - 1, inside, release) + chunk(n - 2, inside, release); }\nfn round(inside: AtomicBool, release: AtomicBool) -> i64 { let result = chunk(8, inside, release); inside.store(false); return result; }\nasync fn churn(inside: AtomicBool, release: AtomicBool, done: AtomicBool) -> i64 { let mut s = 0; while !release.load() { s = round(inside, release); } done.store(true); return s; }\nasync fn main() { let inside = AtomicBool::new(false); let release = AtomicBool::new(false); let done = AtomicBool::new(false); let t = churn(inside, release, done); while !inside.load() { await yield(); } println(7); println(done.load()); release.store(true); await t; }";
    let (out, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_TIME_QUANTUM_MS", "5"),
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
        std::time::Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\nfalse\n");
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
    // Stage A-prime: recursion contains no loop but still runs
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
        "async fn churn() -> String { let mut s = \"\"; for i in 0..50 { s = s + \"x\"; } return s; }\nasync fn main() { let t = churn(); println(await t == \"\"); }",
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
        "async fn spin(started: AtomicBool) -> i64 { let mut i = 0; while true { started.store(true); i = i + 1; } return i; }\nasync fn main() { let started = AtomicBool::new(false); let t = spin(started); while !started.load() { await yield(); } t.cancel(); await t.result(); println(t.is_cancelled()); }",
        &[("WILLOW_TIME_QUANTUM_MS", "5"), ("WILLOW_WORKERS", "1")],
        std::time::Duration::from_secs(25),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn psp_11_sync_helper_countdown_preempts_and_cancels() {
    // loops in synchronous helpers on a task native stack
    // count safepoints inline and enter the runtime once per granted batch.
    // Budget 1 (a grant per safepoint) and the default budget must both keep
    // a single worker fair, and a sync spinner must still observe cancel.
    let source = "fn spin(done: AtomicBool, started: AtomicI64) -> i64 { let mut n = 0; started.add(1); while !done.load() { n = n + 1; } return n; }\nasync fn spinner(done: AtomicBool, started: AtomicI64) -> i64 { return spin(done, started); }\nasync fn release_all(done: AtomicBool, started: AtomicI64) { while started.load() < 3 { await yield(); } println(42); done.store(true); }\nasync fn main() { let done = AtomicBool::new(false); let started = AtomicI64::new(0); let a = spinner(done, started); let b = spinner(done, started); let c = spinner(done, started); await release_all(done, started); await a; await b; await c; let never = AtomicBool::new(false); let t = spinner(never, started); while started.load() < 4 { await yield(); } t.cancel(); await t.result(); println(t.is_cancelled()); }";
    for budget in ["1", "1024"] {
        let (out, ok) = compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", budget)],
            std::time::Duration::from_secs(25),
        );
        assert!(ok, "budget {budget}: {out}");
        assert_eq!(out, "42\ntrue\n", "budget {budget}");
    }
}
