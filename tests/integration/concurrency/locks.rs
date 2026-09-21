use super::*;

// ── Mutex<T> / RwLock<T> (willow-dgwo.3) ─────────────────────────────────────
//
// 20 test perspectives:
//  1. Mutex<i64> get reads the initial value.
//  2. Mutex set then get.
//  3. RwLock<bool> read reads initial.
//  4. RwLock write then read.
//  5. Element type inferred from the constructor argument (i64).
//  6. Element type inferred as bool.
//  7. Element type inferred as f64 (word coercion round-trips).
//  8. Explicit type argument `Mutex<i64>::new(0)`.
//  9. Mutex<String> (GC element) round-trips a value.
// 10. A GC element survives collection (traced via the lock registry).
// 11. Mutex shared across async tasks accumulates correctly.
// 12. Mutex passed as a function parameter.
// 13. get() result usable in arithmetic.
// 14. RwLock<i64> read/write with numbers.
// 15. Mutex::new wrong arg count rejected.
// 16. Explicit type arg mismatch rejected.
// 17. Unknown Mutex method rejected (E0806).
// 18. RwLock has no get/set (only read/write) — unknown method rejected.
// 19. Compiler-known with no import.
// 20. Multiple independent locks.
#[test]
fn test_blocking_cell_get_set() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m = BlockingCell::new(10);
    println(m.get());   // 10
    m.set(25);
    println(m.get());   // 25
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n25\n");
}

#[test]
fn test_blocking_rw_cell_read_write_bool() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = BlockingRwCell::new(true);
    println(r.read());
    r.write(false);
    println(r.read());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn test_blocking_cell_f64_word_coercion() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m = BlockingCell::new(2.5);
    m.set(3.5);
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3.5\n");
}

#[test]
fn test_blocking_cell_explicit_type_arg() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m = BlockingCell<i64>::new(7);
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_blocking_cell_string_survives_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let m = BlockingCell::new("hello");
    let mut i = 0;
    while i < 30 { let junk = BlockingCell::new(i); i = i + 1; }
    gc_collect();
    println(m.get());
    m.set("world");
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\nworld\n");
}

#[test]
fn test_mutex_passed_across_async_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn bump(m: Mutex<i64>, n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        lock m as mut cell { cell = cell + 1; }
        await sleep(1);
        i = i + 1;
    }
    return n;
}
async fn main() {
    let m = Mutex::new(0);
    let a = bump(m, 3);
    await a;
    let b = bump(m, 4);
    await b;
    lock m as cell {
        println(cell);   // 7
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_blocking_cell_param_and_independent_cells() {
    let (out, ok) = compile_and_run(
        r#"
fn add_to(m: BlockingCell<i64>, n: i64) { m.set(m.get() + n); }
fn main() {
    let x = BlockingCell::new(0);
    let y = BlockingCell::new(0);
    add_to(x, 3);
    add_to(y, 100);
    println(x.get() + 1);   // 4
    println(y.get());       // 100
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n100\n");
}

#[test]
fn test_mutex_new_wrong_arg_count_rejected() {
    assert_compile_error_contains(
        "fn main() { let m = Mutex::new(); }\n",
        &["error[E0201]", "expects 1 argument"],
    );
}

#[test]
fn test_mutex_explicit_type_arg_mismatch_rejected() {
    assert_compile_error_contains(
        "fn main() { let m = Mutex<i64>::new(true); }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_mutex_unknown_method_rejected() {
    assert_compile_error_contains(
        "fn main() { let m = Mutex::new(0); m.lock(); }\n",
        &["error[E0806]", "no method `lock`"],
    );
}

#[test]
fn test_rwlock_has_no_get() {
    assert_compile_error_contains(
        "fn main() { let r = RwLock::new(0); r.get(); }\n",
        &["error[E0806]", "no method `get`"],
    );
}

// Case A (willow-h2vf.5): an async fn already returns Task<ReturnType>, so its
// declared return type must be the awaited value, not a task handle (E0809).
#[test]
fn test_async_return_task_handle_rejected_task() {
    assert_compile_error_contains(
        "async fn f() -> Task<i64> { return 1; }\nfn main() {}\n",
        &[
            "error[E0809]",
            "async fn return type must be the awaited value",
        ],
    );
}

#[test]
fn test_async_return_task_handle_rejected_future() {
    assert_compile_error_contains(
        "async fn f() -> Future<i64> { return 1; }\nfn main() {}\n",
        &["error[E0809]"],
    );
}

#[test]
fn test_async_return_task_handle_rejected_join_handle() {
    assert_compile_error_contains(
        "async fn f() -> JoinHandle<i64> { return 1; }\nfn main() {}\n",
        &["error[E0809]"],
    );
}

#[test]
fn test_async_return_plain_value_allowed() {
    // The awaited-value annotation (`-> i64`) is fine and yields an awaitable task.
    let (out, ok) = compile_and_run(
        r#"
async fn f() -> i64 { await sleep(1); return 7; }
async fn main() { println(await f()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_async_call_is_awaitable_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 { await sleep(1); return x * 2; }
async fn main() {
    let t = work(21);
    println(await t);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_async_call_concurrent_awaits_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(id: i64, ticks: i64) -> i64 {
    let mut i = 0;
    while i < ticks { await sleep(1); i = i + 1; }
    return id * 100 + i;
}
async fn main() {
    let a = work(1, 2);
    let b = work(2, 3);
    println(await a);
    println(await b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "102\n203\n");
}

#[test]
fn test_async_call_await_inline_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 { await sleep(1); return x * x; }
async fn main() {
    println(await square(5));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

// `await` resumes when the TARGET task completes, not when the whole scheduler
// drains (willow-bsqy).
#[test]
fn test_await_returns_when_target_completes_not_draining_all() {
    // a completes immediately; b is unrelated and never awaited. With five
    // workers b may start concurrently, but awaiting a must not wait for b's
    // timer.
    let (out, ok) = compile_and_run(
        r#"
async fn a_task() -> i64 { return 1; }
async fn b_task() -> i64 { println(91); await sleep(200); println(92); return 2; }
async fn main() {
    let a = a_task();
    let b = b_task();
    println(await a);
    println(99);
}
"#,
    );
    assert!(ok);
    let lines: Vec<_> = out.lines().collect();
    assert!(lines.contains(&"1"), "{out}");
    assert!(lines.contains(&"99"), "{out}");
    assert!(
        !lines.contains(&"92"),
        "await drained an unrelated task: {out}"
    );
}

#[test]
fn test_unrelated_task_is_still_awaitable_afterwards() {
    // Explicitly awaiting b finishes it; its side effects happen before that
    // await completes.
    let (out, ok) = compile_and_run(
        r#"
async fn a_task() -> i64 { return 1; }
async fn b_task() -> i64 { println(91); await sleep(1); println(92); return 2; }
async fn main() {
    let a = a_task();
    let b = b_task();
    println(await a);
    println(await b);
    println(99);
}
"#,
    );
    assert!(ok);
    let lines: Vec<_> = out.lines().collect();
    for expected in ["1", "91", "92", "2", "99"] {
        assert_eq!(
            lines.iter().filter(|line| **line == expected).count(),
            1,
            "{out}"
        );
    }
    let position = |value| lines.iter().position(|line| *line == value).unwrap();
    assert!(position("91") < position("92"), "{out}");
    assert!(position("92") < position("2"), "{out}");
    assert!(position("2") < position("99"), "{out}");
}

#[test]
fn test_await_drives_target_dependencies() {
    // a awaits c, so awaiting a must still drive c to completion.
    let (out, ok) = compile_and_run(
        r#"
async fn c_task() -> i64 { await sleep(1); return 5; }
async fn a_task() -> i64 { let c = c_task(); return await c + 1; }
async fn main() { let a = a_task(); println(await a); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n");
}

#[test]
fn test_await_does_not_hang_on_unrelated_long_task() {
    // b would run far longer than a; `await a` must return promptly and the
    // program must exit (main awaited only a) rather than draining b.
    let (out, ok) = compile_and_run(
        r#"
async fn quick() -> i64 { await sleep(1); return 42; }
async fn slow() -> i64 {
    let mut i = 0;
    while i < 100000 { await sleep(1); i = i + 1; }
    return i;
}
async fn main() {
    let a = quick();
    let b = slow();
    println(await a);
    println(777);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n777\n");
}

#[test]
fn test_thirty_concurrent_tasks_each_returns_own_value() {
    let (out, ok) = compile_and_run(THIRTY_WORKERS_SRC);
    assert!(ok);
    assert_eq!(out, "0\n4650\n30\n");
}

#[test]
fn test_thirty_concurrent_tasks_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(THIRTY_WORKERS_SRC);
    assert!(ok);
    assert_eq!(out, "0\n4650\n30\n");
}

#[test]
fn test_thirty_concurrent_tasks_sum_465() {
    // Mirrors example/async_concurrent.wi (worker returns id, sum 1..30 = 465).
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
async fn worker(id: i64) -> i64 {
    let mut i = 0;
    let ticks = id % 5 + 1;
    while i < ticks { await sleep(1); i = i + 1; }
    return id;
}
async fn main() {
    let tasks: Array<Task<i64>> = [];
    let mut id = 1;
    while id <= 30 { tasks.push(worker(id)); id = id + 1; }
    let mut total = 0;
    let mut k = 0;
    while k < tasks.len() { total = total + await tasks[k]; k = k + 1; }
    println(total);   // 465
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "465\n");
}

#[test]
fn test_async_9lw_two_concurrent_timers() {
    // Two spawned async workers each loop awaiting sleep; the single-threaded
    // executor drives both concurrently to completion.
    let (stdout, ok) = compile_and_run(
        r#"
async fn worker(id: i64, ticks: i64) -> i64 {
    let mut i = 0;
    while i < ticks {
        await sleep(1);
        i = i + 1;
    }
    return id * 100 + i;
}
async fn main() {
    let a = worker(1, 2);
    let b = worker(2, 3);
    println(await a);
    println(await b);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "102\n203\n");
}

#[test]
fn test_async_9lw_locals_live_across_await() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn main() {
    let mut sum = 0;
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        sum = sum + i;
        i = i + 1;
    }
    println(sum);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "6\n");
}

#[test]
fn test_async_9lw_nested_await_passes_values() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn inner(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn outer(x: i64) -> i64 {
    let a = await inner(x);
    let b = await inner(a);
    return b;
}
async fn main() {
    println(await outer(10));
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "12\n");
}

#[test]
fn test_async_9lw_panic_renders_async_chain() {
    // A panic inside a suspended async fn renders the async future chain
    // (current task first), not just the immediate location — the cooperative
    // scheduler flattens the OS call stack, so this comes from runtime state.
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn inner(x: i64) -> i64 {
    await sleep(1);
    panic("boom in inner");
    return x;
}
async fn main() {
    let r = await inner(5);
    println(r);
}
"#,
    );
    assert!(!ok, "panic must make the program exit non-zero");
    assert!(out.contains("boom in inner"), "panic message: {out}");
    assert!(
        out.contains("async stack"),
        "expected an async stack trace: {out}"
    );
    assert!(out.contains("inner"), "chain should name `inner`: {out}");
    assert!(out.contains("main"), "chain should name `main`: {out}");
}

#[test]
fn test_async_sleep_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn wait_value() -> i64 {
    await sleep(0);
    return 42;
}

async fn main() {
    let value = await wait_value();
    println(value);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_async_task_values_are_awaitable() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn number() -> i64 {
    return 7;
}

async fn flag() -> bool {
    return true;
}

async fn ratio() -> f64 {
    return 2.5;
}

async fn word() -> String {
    return "ok";
}

async fn main() {
    let number_task = number();
    let value = await number_task;
    println(value);
    println(await flag());
    println(await ratio());
    println(await word());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "7\ntrue\n2.5\nok\n");
}

#[test]
fn test_async_mut_reference_parameter_reports_e1707() {
    assert_compile_error_contains(
        r#"
async fn update(x: &mut i64) {
    x = x + 1;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E1707]",
            "reference parameter `x` is not supported in async function",
            "`&mut` parameter may live across suspension points",
        ],
    );
}

#[test]
fn test_async_immutable_reference_parameter_reports_e1707() {
    assert_compile_error_contains(
        r#"
async fn read(x: & i64) -> i64 {
    return x;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E1707]",
            "reference parameter `x` is not supported in async function",
            "`&` parameter may live across suspension points",
        ],
    );
}

#[test]
fn test_task_await_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    return x * 2;
}

async fn main() {
    let h = work(21);
    println(await h);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_spawn_multiple_parallel_tasks_compile_and_run() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 {
    return x * x;
}

async fn main() {
    let a = square(3);
    let b = square(4);
    let c = square(5);
    println(await a);
    println(await b);
    println(await c);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "9\n16\n25\n");
}

#[test]
fn test_await_outside_async_reports_e0801() {
    assert_compile_error_contains(
        r#"
fn value() -> i64 {
    return 1;
}

fn main() {
    await value();
}
"#,
        &[
            "error[E0801]",
            "`await` can only be used inside an async function",
            "`await` used in a non-async function",
            "help: make the enclosing function `async`",
        ],
    );
}

#[test]
fn test_select_block_is_supported() {
    // `select` is implemented (willow-7aj): a ready recv case runs its body.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(5);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(0); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn test_await_non_future_reports_e0803() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let value = await 1;
}
"#,
        &[
            "error[E0803]",
            "cannot await value of type `i64`",
            "expected an awaitable",
        ],
    );
}

#[test]
fn test_looping_sync_helper_in_task_context_reports_e0810() {
    assert_sync_preemption_capability(
        r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

async fn run() -> i64 {
    return heavy(10);
}

async fn main() {
    await run();
}
"#,
        &[
            "error[E0810]",
            "sync helper `heavy` with a loop is not preemptible in task context",
            "this call can monopolize the scheduler worker",
            "help: make the helper async, or wait for task-aware sync-stack preemption support",
        ],
    );
}
