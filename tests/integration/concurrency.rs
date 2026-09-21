use super::support::*;

// ---------------------------------------------------------------------------
// Async state machines + async stack traces — willow-9lw acceptance.
// ---------------------------------------------------------------------------

// WILLOW_WORKERS honors any positive count; the default is available
// parallelism. Results must agree for single and multiple workers.
const WORKERS_CONCURRENT_SRC: &str = r#"
async fn compute(n: i64) -> i64 {
    await sleep(1);
    return n * n;
}
async fn main() {
    let a = compute(1);
    let b = compute(2);
    let c = compute(3);
    println(await a + await b + await c);
}
"#;

// ── Async-call capture checking (willow-dgwo.4) ──────────────────────────────
// Always enforced, independently of WILLOW_WORKERS or WILLOW_DATA_RACE_CHECK.
//
// 20 perspectives (this block + the dgwo.2 classifier unit tests in
// type_checker/send_sync.rs cover the underlying type rules):
//  1. non-Sync GC arg (Array) rejected under the check (E2402)
//  2. a single-worker override still rejects unsafe captures
//  3. E2402 help names the safe wrappers
//  4. Map arg rejected
//  5. Option<Array> arg rejected
//  6. class with an Array field rejected
//  7. Sync class (all-i64 fields) accepted
//  8. Mutex / Channel / Atomic / i64 / String args accepted together
//  9. RwLock<i64> arg accepted
// 10. RwLock<Array<i64>> arg rejected (inner not Sync)
// 11. Mutex<Array<i64>> arg accepted (Mutex needs only inner Send)
// 12. AtomicBool arg accepted
// 13. fieldless enum arg accepted (scalar tag, Send+Sync)
// 14. payload enum carrying an Array rejected
// 15. only the offending arg is flagged (good args alongside)
// 16. a NON-async call passing an Array is NOT checked (no Task boundary)
// 17. passing a Task<T> handle as an arg is rejected (Task is not Sync)
// 18. multiple non-Sync args each report E2402
// 19. scalar-only async fn accepted
// 20. nested async call forwarding a Sync arg is accepted
const NONSYNC_ARG_SRC: &str = r#"
import std::collections::Array;
async fn use_xs(xs: Array<i64>) -> i64 { await sleep(1); return xs[0]; }
async fn main() { let xs: Array<i64> = [1, 2, 3]; println(await use_xs(xs)); }
"#;

// ── Multi-worker capstone (willow-dgwo.9) ─────────────────────────────────────
// Send/Sync checks make task migration sound regardless of worker count.
const NONSEND_ASYNC_FRAME_SRC: &str = r#"
fn inc(x: i64) -> i64 { return x + 1; }
async fn run() -> i64 {
    let op: fn(i64) -> i64 = inc;
    await sleep(1);
    return op(41);
}
async fn main() { println(await run()); }
"#;

// Many concurrent tasks: start 30 async workers, collect handles in an array,
// await them all. Verifies the scheduler + array-of-Task + targeted-await scale
// and that each task keeps its own identity/result (willow-9lw/h2vf/bsqy).
const THIRTY_WORKERS_SRC: &str = r#"
import std::collections::Array;
async fn worker(id: i64) -> i64 {
    let mut i = 0;
    let ticks = id % 5 + 1;   // vary awaits so the 30 tasks interleave
    while i < ticks { await sleep(1); i = i + 1; }
    return id * 10;
}
async fn main() {
    let tasks: Array<Task<i64>> = [];
    let mut id = 1;
    while id <= 30 { tasks.push(worker(id)); id = id + 1; }
    let mut k = 0;
    let mut mismatches = 0;
    let mut total = 0;
    while k < tasks.len() {
        let r = await tasks[k];
        if r != (k + 1) * 10 { mismatches = mismatches + 1; }
        total = total + r;
        k = k + 1;
    }
    println(mismatches);       // 0 — every task matched its expected result
    println(total);            // (1+..+30)*10 = 4650
    println(tasks.len());      // 30
}
"#;

// ── Scheduler stays alive for BlockedSyscall-only workloads (willow-0a6k.5) ─
// With no ready tasks, no timers, and no netpoll waiters, a pending blocking-
// pool job must still keep the scheduler waiting: run_until returning early
// would hand the awaiter an unfinished result. A FIFO makes the I/O genuinely
// slow (the writer arrives 300ms later from the test harness).
#[cfg(target_os = "linux")]
fn libc_o_nonblock() -> i32 {
    // O_NONBLOCK on Linux; avoids a libc dev-dependency for one flag.
    0o4000
}

const RWLOCK_CONTENDED_COUNTER_SRC: &str = r#"
async fn bump(r: RwLock<i64>, times: i64) {
    let mut i = 0;
    while i < times {
        lock write r as mut value { value = value + 1; }
        i = i + 1;
    }
}
async fn read(r: RwLock<i64>) -> i64 {
    lock read r as value { return value; }
}
async fn main() {
    let r = RwLock::new(0);
    let a = bump(r, 250);
    let b = bump(r, 250);
    let c = bump(r, 250);
    let d = bump(r, 250);
    await a; await b; await c; await d;
    println(await read(r));
}
"#;

/// Four tasks x 250 read-modify-writes on one mutex. Reused across worker
/// counts so the only variable is the scheduler's shape.
const CONTENDED_COUNTER_SRC: &str = r#"
async fn bump(m: Mutex<i64>, times: i64) {
    let mut i = 0;
    while i < times {
        lock m as mut value {
            value = value + 1;
        }
        i = i + 1;
    }
}
async fn main() {
    let m = Mutex::new(0);
    let t0 = bump(m, 250);
    let t1 = bump(m, 250);
    let t2 = bump(m, 250);
    let t3 = bump(m, 250);
    await t0;
    await t1;
    await t2;
    await t3;
    lock m as value { println(value); }
}
"#;

/// A GC-managed protected value, published from inside a section and read back
/// after a collection. Reused with and without GC stress.
const GC_MANAGED_LOCK_SRC: &str = r#"
async fn main() {
    let name = Mutex::new("");
    lock name as mut value {
        value = "willow";
    }
    gc_collect();
    lock name as value {
        println(value);
    }
}
"#;

// These child modules preserve leaf test names and the `concurrency::` filter.
#[path = "concurrency/observability.rs"]
mod observability;
#[path = "concurrency/parallel_map.rs"]
mod parallel_map;
#[path = "concurrency/structured.rs"]
mod structured;

// The same source remains rejected on runtimes without native task stacks.
fn assert_sync_preemption_capability(source: &str, expected: &[&str]) {
    if cfg!(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "windows",
            target_env = "msvc",
            target_arch = "x86_64"
        )
    )) {
        let source = if source.contains("fn main(") {
            source.to_owned()
        } else {
            format!("{source}\nfn main() {{}}")
        };
        let (ok, diagnostics) = compile_with_compiler_env(&source, &[]);
        assert!(ok, "{diagnostics}");
    } else {
        assert_compile_error_contains(source, expected);
    }
}

#[path = "concurrency/async_defer.rs"]
mod async_defer;
#[path = "concurrency/async_execution.rs"]
mod async_execution;
#[path = "concurrency/atomics.rs"]
mod atomics;
#[path = "concurrency/await_evaluation.rs"]
mod await_evaluation;
#[path = "concurrency/blocking_cells.rs"]
mod blocking_cells;
#[path = "concurrency/blocking_syscalls.rs"]
mod blocking_syscalls;
#[path = "concurrency/bounded_channels.rs"]
mod bounded_channels;
#[path = "concurrency/cancel_integrity.rs"]
mod cancel_integrity;
#[path = "concurrency/cancellation.rs"]
mod cancellation;
#[path = "concurrency/capture_checking.rs"]
mod capture_checking;
#[path = "concurrency/channel_suspension.rs"]
mod channel_suspension;
#[path = "concurrency/channel_waiters.rs"]
mod channel_waiters;
#[path = "concurrency/completion_diagnostics.rs"]
mod completion_diagnostics;
#[path = "concurrency/frozen_collections.rs"]
mod frozen_collections;
#[path = "concurrency/function_spawn.rs"]
mod function_spawn;
#[path = "concurrency/gc_channels.rs"]
mod gc_channels;
#[path = "concurrency/happens_before.rs"]
mod happens_before;
#[path = "concurrency/imported_async.rs"]
mod imported_async;
#[path = "concurrency/imported_preemption.rs"]
mod imported_preemption;
#[path = "concurrency/lock_restrictions.rs"]
mod lock_restrictions;
#[path = "concurrency/locks.rs"]
mod locks;
#[path = "concurrency/multi_worker.rs"]
mod multi_worker;
#[path = "concurrency/panic_policy.rs"]
mod panic_policy;
#[path = "concurrency/preemption.rs"]
mod preemption;
#[path = "concurrency/safepoints.rs"]
mod safepoints;
#[path = "concurrency/scheduler_locks.rs"]
mod scheduler_locks;
#[path = "concurrency/select_evaluation.rs"]
mod select_evaluation;
#[path = "concurrency/select_hardening.rs"]
mod select_hardening;
#[path = "concurrency/select_timeouts.rs"]
mod select_timeouts;
#[path = "concurrency/send_sync.rs"]
mod send_sync;
#[path = "concurrency/spawn_types.rs"]
mod spawn_types;
#[path = "concurrency/task_send.rs"]
mod task_send;
#[path = "concurrency/task_status.rs"]
mod task_status;
#[path = "concurrency/task_traces.rs"]
mod task_traces;
