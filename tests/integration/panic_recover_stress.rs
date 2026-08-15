use super::support::*;
use std::time::Duration;

// Panic/recover acceptance and stress suite — willow-s9ej.7 (Stage 6).
//
// Stages 2-5 pin the *semantics* of lexical unwinding and recovery. This file
// pins that those semantics survive the environments that break rooting,
// scheduling, and cleanup bookkeeping: GC stress in every mode, real worker
// pools, preemption, tiny task budgets, cancellation, and unhandled-panic
// reporting. Every scheduling test runs under a hard deadline, so a lost wakeup
// or a cleanup deadlock fails the suite instead of hanging it.
//
// 30 perspectives:
//   1  allocation stress keeps a synchronous recovery loop correct
//   2  minor-collection stress keeps the same loop correct
//   3  every stress mode at once keeps the same loop correct
//   4  await-boundary stress keeps async recovery correct
//   5  scheduler-operation stress keeps async recovery correct
//   6  every stress mode at once keeps async recovery correct
//   7  a four-worker pool recovers tasks independently
//   8  an eight-worker pool under allocation stress stays isolated
//   9  preemption at a 1ms quantum does not disturb recovery
//  10  a one-poll task budget re-polls across the recovery continuation
//  11  a long recovery loop terminates and stays bounded
//  12  a nested panic during unwinding is recoverable, the outer one still runs
//  13  a recovered PanicInfo keeps every field across later collections
//  14  each defer runs exactly once per panic under stress
//  15  a deep call chain propagates one panic to the recovery scope
//  16  interface dispatch propagates under a worker pool
//  17  a constructor panic recovers under allocation stress
//  18  a cross-module call propagates and recovers
//  19  a function value (lambda) propagates to its caller's scope
//  20  allocation-heavy recovery keeps generated root depth balanced
//  21  strict await of a cancelled task is a recoverable panic under stress
//  22  cancellation cleanup stays non-panic while a sibling recovers
//  23  a channel peer keeps making progress after its producer recovers
//  24  sequential worker reuse never bleeds panic state between tasks
//  25  an unhandled synchronous panic keeps its call-stack chain and aborts
//  26  an unhandled async panic keeps its async chain and aborts
//  27  defers still run before an unhandled panic is reported
//  28  a nested unhandled panic reports both records
//  29  an unhandled panic on a worker pool aborts before waking its awaiters
//  30  release builds behave exactly like debug builds

/// Hard deadline for scheduling/stress runs. GC stress multiplies work by a
/// large constant, so this is generous; the point is that a deadlock fails.
const STRESS_TIMEOUT: Duration = Duration::from_secs(180);

/// Synchronous recovery inside a loop: one iteration panics, the rest complete.
const SYNC_LOOP: &str = r#"
fn helper(n: i64) -> i64 {
    if n == 3 {
        panic("helper-{}", n);
    }
    return n * 2;
}

fn main() {
    let mut total = 0;
    for i in 0..6 {
        if true {
            defer match recover() {
                Some(info) => println(info.message),
                None => {}
            }
            total = total + helper(i);
        }
    }
    println(total);
}
"#;

/// Async recovery across a suspension: the recovered task resumes after its
/// scope and still awaits normally.
const ASYNC_LOOP: &str = r#"
async fn worker(tag: i64) -> i64 {
    let mut status = 0;
    if true {
        defer match recover() {
            Some(_) => {},
            None => {}
        }
        if tag % 3 == 0 {
            panic("boom-{}", tag);
        }
        status = 1;
    }
    await sleep(1);
    return status;
}

async fn main() {
    let mut total = 0;
    for i in 0..6 {
        total = total + await worker(i);
    }
    println(total);
}
"#;

fn run_stress(source: &str, env: &[(&str, &str)]) -> String {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(source, env, STRESS_TIMEOUT);
    assert!(
        !timed_out,
        "run did not finish under {STRESS_TIMEOUT:?}: {out}"
    );
    assert!(ok, "run failed: {out}");
    out
}

/// Perspective 1.
#[test]
fn psr_01_sync_recovery_loop_under_allocation_stress() {
    let out = run_stress(SYNC_LOOP, &[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(out, "helper-3\n24\n");
}

/// Perspective 2.
#[test]
fn psr_02_sync_recovery_loop_under_minor_collection_stress() {
    let out = run_stress(SYNC_LOOP, &[("WILLOW_GC_STRESS", "minor")]);
    assert_eq!(out, "helper-3\n24\n");
}

/// Perspective 3.
#[test]
fn psr_03_sync_recovery_loop_under_every_stress_mode() {
    let out = run_stress(SYNC_LOOP, &[("WILLOW_GC_STRESS", "all")]);
    assert_eq!(out, "helper-3\n24\n");
}

/// Perspective 4.
#[test]
fn psr_04_async_recovery_under_await_boundary_stress() {
    let out = run_stress(ASYNC_LOOP, &[("WILLOW_GC_STRESS", "await")]);
    assert_eq!(out, "4\n");
}

/// Perspective 5.
#[test]
fn psr_05_async_recovery_under_scheduler_operation_stress() {
    let out = run_stress(ASYNC_LOOP, &[("WILLOW_GC_STRESS", "scheduler")]);
    assert_eq!(out, "4\n");
}

/// Perspective 6.
#[test]
fn psr_06_async_recovery_under_every_stress_mode() {
    let out = run_stress(ASYNC_LOOP, &[("WILLOW_GC_STRESS", "all")]);
    assert_eq!(out, "4\n");
}

/// Concurrent recovery: six tasks are spawned before any is awaited, so they
/// interleave on the pool. Each reports its own outcome through its return
/// value, keeping the program's output deterministic.
const PARALLEL_WORKERS: &str = r#"
async fn worker(tag: i64) -> i64 {
    let mut status = 0;
    if true {
        defer match recover() {
            Some(info) => {
                if info.message == "boom-" + tag.toString() {
                    status = 100;
                }
            },
            None => {}
        }
        if tag % 3 == 0 {
            panic("boom-{}", tag);
        }
        status = 1;
    }
    await sleep(1);
    return status;
}

async fn main() {
    let a = worker(0);
    let b = worker(1);
    let c = worker(2);
    let d = worker(3);
    let e = worker(4);
    let f = worker(5);
    let mut total = 0;
    total = total + await a;
    total = total + await b;
    total = total + await c;
    total = total + await d;
    total = total + await e;
    total = total + await f;
    println(total);
}
"#;

/// Perspective 7. Every recovering task must see ITS OWN payload: a bled
/// context would give a task the neighbour's message and drop the total.
#[test]
fn psr_07_four_worker_pool_recovers_tasks_independently() {
    let out = run_stress(PARALLEL_WORKERS, &[("WILLOW_WORKERS", "4")]);
    assert_eq!(out, "204\n");
}

/// Perspective 8.
#[test]
fn psr_08_eight_worker_pool_under_allocation_stress() {
    let out = run_stress(
        PARALLEL_WORKERS,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert_eq!(out, "204\n");
}

/// Perspective 9. A CPU-bound recovering task is preempted at safepoints while
/// its panic-defer bookkeeping is live.
#[test]
fn psr_09_preemption_does_not_disturb_recovery() {
    let source = r#"
async fn spin(tag: i64) -> i64 {
    let mut sum = 0;
    if true {
        defer match recover() {
            Some(_) => {},
            None => {}
        }
        let mut i = 0;
        while i < 200000 {
            sum = sum + i;
            i = i + 1;
        }
        panic("spun-{}", tag);
    }
    return sum;
}

async fn main() {
    let first = spin(1);
    let second = spin(2);
    println(await first + await second);
}
"#;
    let out = run_stress(
        source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_TIME_QUANTUM_MS", "1")],
    );
    assert_eq!(out, "39999800000\n");
}

/// Perspective 10. A one-poll budget forces the scheduler to re-enter the task
/// between the recovery scope and its continuation.
#[test]
fn psr_10_single_poll_task_budget_spans_the_recovery_continuation() {
    let source = r#"
async fn work() -> i64 {
    let mut value = 0;
    if true {
        defer match recover() {
            Some(_) => {},
            None => {}
        }
        await sleep(1);
        panic("budgeted");
    }
    await sleep(1);
    value = 7;
    return value;
}

async fn main() {
    println(await work());
}
"#;
    let out = run_stress(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "await")],
    );
    assert_eq!(out, "7\n");
}

/// Perspective 11. Repeated recovery must not accumulate roots, contexts, or
/// defer state; a leak shows up as a timeout or an abort under stress.
#[test]
fn psr_11_long_recovery_loop_terminates_and_stays_bounded() {
    let source = r#"
fn maybe_fail(n: i64) -> i64 {
    if n % 2 == 0 {
        panic("iteration-{}", n);
    }
    return n;
}

fn main() {
    let mut recovered = 0;
    let mut completed = 0;
    for i in 0..300 {
        if true {
            defer match recover() {
                Some(info) => {
                    if info.line > 0 {
                        recovered = recovered + 1;
                    }
                },
                None => {}
            }
            completed = completed + maybe_fail(i);
        }
    }
    println(recovered);
    println(completed);
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "minor")]);
    assert_eq!(out, "150\n22500\n");
}

/// Perspective 12. A panic raised while a defer is unwinding becomes the
/// current panic, but it does not erase the one already unwinding: `recover()`
/// consumes only the topmost record, so the outer scope takes "second" while
/// "first" keeps unwinding and eventually aborts the process (spec §9/§21).
#[test]
fn psr_12_nested_panic_during_unwind_consumes_only_the_topmost_record() {
    let source = r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer got " + info.message),
            None => {}
        }
        if true {
            defer panic("second");
            panic("first");
        }
        println("unreachable");
    }
    println("after");
}
"#;
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "the still-active outer panic must abort: {out}");
    assert!(out.starts_with("outer got second\n"), "{out}");
    assert!(out.contains("runtime panic: first"), "{out}");
    assert!(!out.contains("unreachable"), "{out}");
    assert!(!out.contains("after"), "{out}");
}

/// Perspective 13. The recovered payload is ordinary GC-visible data once the
/// deferred block owns it, so later allocations must not free it.
#[test]
fn psr_13_recovered_payload_survives_later_collections() {
    let source = r#"
fn fail() {
    panic("payload-{}", 42);
}

fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                let mut noise = "";
                let mut mirror = "";
                for i in 0..64 {
                    noise = noise + "x";
                    mirror = mirror + "x";
                }
                println(info.message);
                println(info.line > 0);
                println(info.column > 0);
                println(noise == mirror);
            },
            None => {}
        }
        fail();
    }
    println("after");
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(out, "payload-42\ntrue\ntrue\ntrue\nafter\n");
}

/// Perspective 14. Exactly-once: three defers in one scope run once each, in
/// LIFO order, and none of them re-runs after recovery. The `third` defer also
/// pins down argument timing: defer arguments are evaluated at REGISTRATION,
/// so it reports `runs` as it was before the body incremented it.
#[test]
fn psr_14_every_defer_runs_exactly_once_under_stress() {
    let source = r#"
fn main() {
    let mut runs = 0;
    if true {
        defer println("third " + runs.toString());
        defer match recover() {
            Some(_) => println("second"),
            None => println("second-none")
        }
        defer println("first");
        runs = runs + 1;
        panic("once");
    }
    println("after");
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "all")]);
    assert_eq!(out, "first\nsecond\nthird 0\nafter\n");
}

/// Perspective 15.
#[test]
fn psr_15_deep_call_chain_propagates_one_panic() {
    let source = r#"
fn deep(n: i64) -> i64 {
    if n == 0 {
        panic("bottom");
    }
    return deep(n - 1) + 1;
}

fn main() {
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => {}
        }
        println(deep(32));
    }
    println("after");
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(out, "bottom\nafter\n");
}

/// Perspective 16. Interface (dynamic) dispatch is an ordinary call for panic
/// propagation: the panic crosses the vtable call and lands in the async
/// caller's recovery scope. The interface value stays inside a sync helper
/// because interface values are not `Send` and may live in no task frame
/// (E2402/E2404).
#[test]
fn psr_16_interface_dispatch_propagates_on_a_worker_pool() {
    let source = r#"
interface Handler {
    fn handle(self, input: i64) -> i64;
}

class Doubler implements Handler {
    pub fn handle(self, input: i64) -> i64 {
        if input < 0 {
            panic("negative input");
        }
        return input * 2;
    }
}

fn dispatch(input: i64) -> i64 {
    let handler: Handler = new Doubler();
    return handler.handle(input);
}

async fn serve(input: i64) -> i64 {
    let mut value = -1;
    if true {
        defer match recover() {
            Some(_) => {},
            None => {}
        }
        value = dispatch(input);
    }
    await sleep(1);
    return value;
}

async fn main() {
    let good = serve(21);
    let bad = serve(-1);
    println(await good);
    println(await bad);
}
"#;
    let out = run_stress(
        source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "scheduler")],
    );
    assert_eq!(out, "42\n-1\n");
}

/// Perspective 17. A constructor that panics leaves no half-built object
/// visible to the recovery continuation.
#[test]
fn psr_17_constructor_panic_recovers_under_allocation_stress() {
    let source = r#"
class Config {
    pub name: String;

    pub init(self, name: String) {
        if name == "" {
            panic("empty config name");
        }
        self.name = name;
    }
}

fn main() {
    let mut built = "none";
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => {}
        }
        let config = new Config("");
        built = config.name;
    }
    println(built);
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(out, "empty config name\nnone\n");
}

/// Perspective 18. Module boundaries are ordinary calls for panic propagation.
#[test]
fn psr_18_cross_module_panic_recovers_in_the_caller() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "validate.wi",
                r#"
pub fn check(value: i64) -> i64 {
    if value < 0 {
        panic("negative value: {}", value);
    }
    return value;
}
"#,
            ),
            (
                "main.wi",
                r#"
import validate;

fn main() {
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => {}
        }
        println(validate::check(-5));
    }
    println(validate::check(5));
}
"#,
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "negative value: -5\n5\n");
}

/// Perspective 19.
#[test]
fn psr_19_function_value_panic_reaches_the_callers_scope() {
    let source = r#"
fn main() {
    let checker = |value: i64| -> i64 {
        if value == 0 {
            panic("zero not allowed");
        }
        return 100 / value;
    };
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => {}
        }
        println(checker(4));
        println(checker(0));
        println("unreachable");
    }
    println("after");
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(out, "25\nzero not allowed\nafter\n");
}

/// Perspective 20. Allocation inside the panicking scope makes the shadow-root
/// depth path dependent; unwinding must restore the scope's entry depth rather
/// than pop a fixed count.
#[test]
fn psr_20_allocation_heavy_recovery_keeps_root_depth_balanced() {
    let source = r#"
fn build(n: i64) -> String {
    let mut text = "row-" + n.toString();
    if n % 4 == 3 {
        panic("bad row " + text);
    }
    text = text + "!";
    return text;
}

fn main() {
    let mut rows = 0;
    for i in 0..40 {
        if true {
            defer match recover() {
                Some(_) => {},
                None => {}
            }
            let first = build(i);
            let second = build(i) + first;
            if second == first + first {
                rows = rows + i;
            }
        }
    }
    println(rows);
}
"#;
    let out = run_stress(source, &[("WILLOW_GC_STRESS", "alloc")]);
    // Rows 3, 7, ... 39 panic before the accumulation; the other rows add their
    // own index: sum(0..39) == 780 minus sum(3, 7, ... 39) == 210.
    assert_eq!(out, "570\n");
}

/// Perspective 21. Cancellation stays a terminal task state; only strict await
/// of a cancelled task raises a recoverable language panic.
#[test]
fn psr_21_strict_await_of_cancelled_task_is_recoverable_under_stress() {
    let source = r#"
async fn slow() -> i64 {
    await sleep(5000);
    return 1;
}

async fn main() {
    let task = slow();
    await sleep(5);
    task.cancel();
    if true {
        defer match recover() {
            Some(info) => println("recovered"),
            None => {}
        }
        println(await task);
    }
    println(task.is_cancelled());
}
"#;
    let out = run_stress(
        source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "scheduler")],
    );
    assert_eq!(out, "recovered\ntrue\n");
}

/// Perspective 22. A cancelled task's own defer cleanup is not a panic, and it
/// runs while another task is recovering on the same pool.
#[test]
fn psr_22_cancel_cleanup_stays_non_panic_beside_a_recovering_task() {
    let source = r#"
async fn cleaned() {
    defer println("cleaned");
    await sleep(5000);
}

async fn recovering() -> i64 {
    let mut value = 0;
    if true {
        defer match recover() {
            Some(_) => {},
            None => {}
        }
        panic("sibling");
    }
    await sleep(1);
    value = 5;
    return value;
}

async fn main() {
    let task = cleaned();
    let peer = recovering();
    await sleep(10);
    task.cancel();
    println(await peer);
    await sleep(30);
    println(task.is_cancelled());
}
"#;
    let out = run_stress(source, &[("WILLOW_WORKERS", "4")]);
    assert_eq!(out, "5\ncleaned\ntrue\n");
}

/// Perspective 23.
#[test]
fn psr_23_channel_peer_progresses_after_the_producer_recovers() {
    let source = r#"
async fn producer(ch: Channel<i64>) {
    if true {
        defer match recover() {
            Some(_) => {},
            None => {}
        }
        panic("producer-fault");
    }
    let mut i = 0;
    while i < 3 {
        ch.send(i);
        i = i + 1;
    }
    ch.close();
}

async fn main() {
    let ch = Channel<i64>::with_capacity(1);
    let task = producer(ch);
    let mut total = 0;
    let mut received = 0;
    while received < 3 {
        total = total + ch.recv();
        received = received + 1;
    }
    await task;
    println(total);
}
"#;
    let out = run_stress(
        source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "scheduler")],
    );
    assert_eq!(out, "3\n");
}

/// Perspective 24. Sequential tasks reuse the same workers; a leaked context
/// would let a later task observe the earlier task's panic.
#[test]
fn psr_24_sequential_worker_reuse_does_not_bleed_panic_state() {
    let source = r#"
async fn request(id: i64) -> i64 {
    let mut handled = 0;
    if true {
        defer match recover() {
            Some(info) => {
                if info.message == "fault-" + id.toString() {
                    handled = 1;
                }
            },
            None => {
                handled = 2;
            }
        }
        if id % 2 == 0 {
            panic("fault-{}", id);
        }
    }
    await sleep(1);
    return handled;
}

async fn main() {
    let mut own = 0;
    let mut clean = 0;
    for i in 0..12 {
        let result = await request(i);
        if result == 1 {
            own = own + 1;
        }
        if result == 2 {
            clean = clean + 1;
        }
    }
    println(own);
    println(clean);
}
"#;
    let out = run_stress(source, &[("WILLOW_WORKERS", "4")]);
    // Six even ids recover their own payload; the six odd ids run the deferred
    // match with no panic at all.
    assert_eq!(out, "6\n6\n");
}

/// Perspective 25.
#[test]
fn psr_25_unhandled_sync_panic_keeps_its_call_stack_and_aborts() {
    let source = r#"
fn level3() {
    panic("deep-boom");
}

fn level2() {
    level3();
}

fn level1() {
    level2();
}

fn main() {
    println("start");
    level1();
}
"#;
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "an unhandled panic must abort: {out}");
    assert!(out.starts_with("start\n"), "{out}");
    assert!(out.contains("runtime panic: deep-boom"), "{out}");
    assert!(out.contains("call stack (most recent call first)"), "{out}");
    for frame in ["level3", "level2", "level1"] {
        assert!(out.contains(frame), "missing frame {frame} in: {out}");
    }
}

/// Perspective 26.
#[test]
fn psr_26_unhandled_async_panic_keeps_its_async_chain_and_aborts() {
    let source = r#"
fn helper() {
    panic("async-deep");
}

async fn work() {
    await sleep(1);
    helper();
}

async fn main() {
    println("start");
    await work();
    println("unreachable");
}
"#;
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "an unhandled async panic must abort: {out}");
    assert!(!out.contains("unreachable"), "{out}");
    assert!(out.contains("runtime panic: async-deep"), "{out}");
    assert!(out.contains("async stack (current task first)"), "{out}");
    assert!(out.contains("async work"), "{out}");
    assert!(out.contains("async main"), "{out}");
}

/// Perspective 27. Reporting happens after unwinding, so cleanup still runs
/// even when nothing recovers.
#[test]
fn psr_27_defers_run_before_an_unhandled_panic_is_reported() {
    let source = r#"
fn main() {
    defer println("outer cleanup");
    if true {
        defer println("inner cleanup");
        panic("no recovery");
    }
}
"#;
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "{out}");
    let inner = out.find("inner cleanup").expect("inner defer must run");
    let outer = out.find("outer cleanup").expect("outer defer must run");
    let report = out.find("runtime panic: no recovery").expect("report");
    assert!(inner < outer, "defers must unwind LIFO: {out}");
    assert!(outer < report, "cleanup precedes reporting: {out}");
}

/// Perspective 28.
#[test]
fn psr_28_nested_unhandled_panic_reports_both_records() {
    let source = r#"
fn main() {
    if true {
        defer println("cleanup");
        defer panic("second");
        panic("first");
    }
}
"#;
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "{out}");
    assert!(out.contains("cleanup"), "{out}");
    assert!(out.contains("runtime panic: second"), "{out}");
    assert!(out.contains("while unwinding panic: first"), "{out}");
}

/// Perspective 29. One task's unhandled panic is process-fatal; a sibling must
/// not silently "win" and let the program exit successfully, and no task may
/// run past `await <panicked task>` — the worker reports and aborts before it
/// publishes the terminal state that would wake the awaiter.
///
/// The captured output merges stdout and stderr, so a sibling's line can land
/// inside the report; assert on fragments rather than on whole lines.
#[test]
fn psr_29_unhandled_panic_on_a_worker_pool_aborts_the_process() {
    let source = r#"
async fn faulty() {
    await sleep(1);
    panic("pool-fault");
}

async fn healthy() -> i64 {
    await sleep(1);
    return 1;
}

async fn main() {
    let bad = faulty();
    let good = healthy();
    println(await good);
    await bad;
    println("unreachable");
}
"#;
    let (out, ok, timed_out) =
        compile_and_run_with_env_timeout(source, &[("WILLOW_WORKERS", "4")], STRESS_TIMEOUT);
    assert!(!timed_out, "unhandled pool panic hung: {out}");
    assert!(!ok, "an unhandled task panic must fail the process: {out}");
    assert!(out.contains("runtime panic:"), "{out}");
    assert!(out.contains("pool-fault"), "{out}");
    assert!(out.contains("async stack (current task first)"), "{out}");
    assert!(
        !out.contains("unreachable"),
        "a task resumed past `await` on a fatally panicked task: {out}"
    );
}

/// Perspective 30. Release builds keep language safety, so recovery behaves
/// identically with optimizations on.
#[test]
fn psr_30_release_build_matches_debug_behavior() {
    const EXPECTED: &str = concat!(
        "sync requests:\n",
        "  finished request 1\n",
        "200 body=42\n",
        "  finished request 2\n",
        "  recovered: invalid payload -7 in request 2\n",
        "500\n",
        "  finished request 3\n",
        "200 body=10\n",
        "async requests:\n",
        "200 body=20\n",
        "500 recovered: invalid payload -1 in request 5\n",
        "200 body=6\n",
    );

    let example = "example/panic_recover_service.wi";
    let (debug_out, debug_ok) = compile_file_and_run(example);
    assert!(debug_ok, "{debug_out}");
    let (release_out, release_ok) = compile_file_and_run_with_args(example, &["--release"]);
    assert!(release_ok, "{release_out}");
    assert_eq!(
        debug_out, EXPECTED,
        "debug transcript or line endings diverged"
    );
    assert_eq!(
        release_out, EXPECTED,
        "release transcript or line endings diverged"
    );
}
