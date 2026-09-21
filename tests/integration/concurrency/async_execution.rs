use super::*;

#[test]
fn test_async_await_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn work() -> i64 {
    return 42;
}

async fn main() {
    let value = await work();
    println(value);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_workers_default_runs_concurrent_program() {
    let (out, ok) = compile_and_run(WORKERS_CONCURRENT_SRC);
    assert!(ok);
    assert_eq!(out, "14\n"); // 1 + 4 + 9
}

#[test]
fn test_workers_env_does_not_change_result() {
    // 1 and 4 are honored; 0 and garbage use available parallelism.
    for value in ["1", "4", "0", "not-a-number"] {
        let (out, ok) =
            compile_and_run_with_env(WORKERS_CONCURRENT_SRC, &[("WILLOW_WORKERS", value)]);
        assert!(ok, "WILLOW_WORKERS={value} should run");
        assert_eq!(out, "14\n", "WILLOW_WORKERS={value} changed the result");
    }
}

#[test]
fn test_workers_high_count_still_correct_under_gc_stress() {
    let (out, ok) = compile_and_run_with_env(
        WORKERS_CONCURRENT_SRC,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "high worker count under GC stress should run");
    assert_eq!(out, "14\n");
}

#[test]
fn async_frame_call_safepoints_narrow_straight_line_locals() {
    // Each value is read by the following arithmetic statement, but neither
    // statement executes a call. With willow-mpvo there is no preemption edge
    // between them, so only `total`, which is live at the final println call,
    // needs a frame slot.
    let mut source = String::from("async fn oversized() {\n    let mut total: i64 = 0;\n");
    for index in 0..1019 {
        source.push_str(&format!("    let value_{index}: i64 = {index};\n"));
        source.push_str(&format!("    total = total + value_{index};\n"));
    }
    source.push_str("    println(total);\n}\nfn main() {}\n");

    let (ok, stderr) = compile_with_compiler_env(&source, &[]);
    assert!(ok, "narrowed frame must compile: {stderr}");
    assert!(
        !stderr.contains("warning[W0801]"),
        "straight-line locals must stay out of the frame: {stderr}"
    );

    // The escape hatch still restores the old frame-everything layout:
    // `total` + 1019 values + 2 fixed slots + the 3-word header = 8200 bytes.
    let (ok, stderr) = compile_with_compiler_env(&source, &[("WILLOW_ASYNC_FRAME_ALL", "1")]);
    assert!(ok, "frame-everything override must compile: {stderr}");
    assert!(
        stderr.contains("async frame for `oversized` is large: 8200 bytes"),
        "stderr: {stderr}"
    );
}

/// willow-lpn.10: `example/async_frame_narrowing.wi` walks through every shape
/// the frame-slot analysis distinguishes (dead-before-await, live-across-await,
/// the loop back edge, branch union, shadowing, and GC-managed locals both read
/// across a suspension and never read at all).
///
/// The narrowing is a codegen decision with no source-level meaning, so the
/// example must produce byte-identical output with it on and with
/// `WILLOW_ASYNC_FRAME_ALL=1` turning it off.
#[test]
fn async_frame_narrowing_example_output_is_independent_of_framing() {
    let source = include_str!("../../../example/async_frame_narrowing.wi");
    const EXPECTED: &str = "2\n1\n102\n13\n12\n3\n30\ntask\n4\n7\nhello\n6\n";

    let (out, ok) = compile_and_run_with_env(source, &[]);
    assert!(ok, "narrowed build must run: {out}");
    assert_eq!(out, EXPECTED, "narrowed output");

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_ASYNC_FRAME_ALL", "1")]);
    assert!(ok, "frame-everything build must run: {out}");
    assert_eq!(out, EXPECTED, "un-narrowed output");
}

/// willow-lpn.10: the same 1020 locals, but never read after their declaration,
/// are dead at every suspension point and get no frame slot at all — so the
/// frame stays tiny and the large-frame warning does not fire.
///
/// `WILLOW_ASYNC_FRAME_ALL=1` opts back out of the narrowing and must restore
/// the warning, which is what makes this a test of the analysis rather than of
/// the warning threshold.
#[test]
fn async_frame_narrowing_drops_locals_that_are_dead_at_every_suspension() {
    let mut source = String::from("async fn oversized() {\n");
    for index in 0..1020 {
        source.push_str(&format!("    let value_{index}: i64 = {index};\n"));
    }
    source.push_str("}\nfn main() {}\n");

    let (ok, stderr) = compile_with_compiler_env(&source, &[]);
    assert!(ok, "narrowed frame must still compile: {stderr}");
    assert!(
        !stderr.contains("warning[W0801]"),
        "dead locals must not be frame-backed: {stderr}"
    );

    let (ok, stderr) = compile_with_compiler_env(&source, &[("WILLOW_ASYNC_FRAME_ALL", "1")]);
    assert!(ok, "frame-everything override must still compile: {stderr}");
    assert!(
        stderr.contains("async frame for `oversized` is large: 8200 bytes"),
        "override must restore the un-narrowed frame: {stderr}"
    );
}

/// A GC local in the final statement of a cooperative poll segment is dead for
/// value-liveness purposes and therefore uses a native stack root. The terminal
/// Ready path and inner-scope fallthrough must pop that root (willow-p42j).
/// Before the fix, repeated calls aborted under allocation stress with
/// "invalid GC pointer in GC root graph".
#[test]
fn coop_shadow_roots_unwind_on_scope_and_terminal_ready() {
    let source = r#"
async fn leak_root() {
    await sleep(0);
    if true {
        let branch_dead = "branch" + " root";
    }
    let terminal_dead = "terminal" + " root";
}

async fn main() {
    let mut i = 0;
    while i < 64 {
        await leak_root();
        let allocation = "still" + " alive";
        if i == 63 {
            println(allocation);
        }
        i = i + 1;
    }
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_WORKERS", "1")],
    );
    assert!(ok, "terminal GC local left a dangling shadow root: {out}");
    assert_eq!(out, "still alive\n");
}

/// willow-p42j: a large suffix of GC locals can be dead at an explicit await
/// while remaining lexically in scope. Pending and budget-preemption returns
/// must pop every native-slot root; dispatch restores zeroed slots before the
/// next poll. A collection immediately after resume used to walk addresses in
/// the destroyed prior poll stack.
#[test]
fn coop_shadow_roots_unwind_and_restore_across_poll_returns() {
    let mut source = String::from("async fn parked() {\n");
    for index in 0..64 {
        source.push_str(&format!(
            "    let dead_{index}: String = \"left\" + \"{index}\";\n"
        ));
    }
    source.push_str(
        r#"
    await sleep(0);
    let after = "still" + " alive";
    println(after);
}

async fn main() {
    await parked();
}
"#,
    );

    let (out, ok) = compile_and_run_with_env(
        &source,
        &[
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_WORKERS", "1"),
        ],
    );
    assert!(ok, "poll-return shadow roots were not balanced: {out}");
    assert_eq!(out, "still alive\n");
}

/// willow-p42j: match block arms use the general `emit_block` path rather than
/// `emit_coop_stmts`. Its runtime roots, scalar count, and cooperative binding
/// tracker must all return to the arm-entry depth before the following await.
#[test]
fn coop_shadow_roots_match_block_restores_tracker_before_await() {
    let source = r#"
async fn matched() {
    match 1 {
        1 => {
            let dead = "match" + " root";
        }
        _ => {}
    }

    await sleep(0);
    let after = "match" + " ok";
    println(after);
}

async fn main() {
    await matched();
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_WORKERS", "1"),
        ],
    );
    assert!(
        ok,
        "match block corrupted the cooperative root tracker: {out}"
    );
    assert_eq!(out, "match ok\n");
}

/// A deferred block is emitted through the same general block path when the
/// async function exits. A dead GC local created by the cleanup must leave both
/// root bookkeeping structures balanced before the terminal Ready return.
#[test]
fn coop_shadow_roots_defer_block_restores_tracker_before_ready() {
    let source = r#"
async fn deferred() {
    defer {
        let dead = "defer" + " root";
    }
    await sleep(0);
    return;
}

async fn main() {
    await deferred();
    let after = "defer" + " ok";
    println(after);
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_WORKERS", "1"),
        ],
    );
    assert!(
        ok,
        "defer block corrupted the cooperative root tracker: {out}"
    );
    assert_eq!(out, "defer ok\n");
}

/// The p42j root protocol is what makes lpn.10 narrowing legal for GC locals,
/// not only scalar locals. More dead GC locals than the 61-bit frame reference
/// mask can represent must compile by staying in balanced stack-root slots. The
/// frame-everything override also compiles, using the scalable bitmap when
/// narrowing is disabled.
#[test]
fn async_frame_narrowing_drops_dead_gc_locals_beyond_frame_mask_capacity() {
    let mut source = String::from("async fn oversized() {\n");
    for index in 0..64 {
        source.push_str(&format!("    let value_{index}: String = \"dead\";\n"));
    }
    source.push_str("    await sleep(0);\n}\nfn main() {}\n");

    let (ok, stderr) = compile_with_compiler_env(&source, &[]);
    assert!(ok, "narrowed GC frame must compile: {stderr}");

    let (ok, stderr) = compile_with_compiler_env(&source, &[("WILLOW_ASYNC_FRAME_ALL", "1")]);
    assert!(ok, "wide frames must compile with bitmap tracing: {stderr}");
}

#[test]
fn preempt_loop_backedge_allows_ready_task_to_run() {
    // More busy tasks than workers must reach their loop before the quick
    // task releases them. This proves progress without assuming print order
    // between independently scheduled finite tasks.
    let source = r#"
async fn cpu_bound(started: AtomicI64, done: AtomicBool) {
    started.add(1);
    while !done.load() { }
}
async fn quick(done: AtomicBool) {
    println(2);
    done.store(true);
}
async fn main() {
    let started = AtomicI64::new(0);
    let done = AtomicBool::new(false);
    let a = cpu_bound(started, done); let b = cpu_bound(started, done);
    let c = cpu_bound(started, done); let d = cpu_bound(started, done);
    let e = cpu_bound(started, done); let f = cpu_bound(started, done);
    while started.load() < 6 { await yield(); }
    await quick(done);
    await a; await b; await c; await d; await e; await f;
    println(1);
}
"#;

    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "loop backedges failed to yield: {out}");
    assert!(ok, "tiny-budget preemption program should run: {out}");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn preempt_async_spin_does_not_starve_timer_task() {
    let source = r#"
async fn spin(done: AtomicBool) -> i64 {
    while !done.load() {
    }
    return 0;
}
async fn delayed(done: AtomicBool) -> i64 {
    await sleep(1);
    println(42);
    done.store(true);
    return 0;
}
async fn main() {
    let done = AtomicBool::new(false);
    let background = spin(done);
    await delayed(done);
    await background;
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(ok, "background CPU spin must not starve the timer task");
    assert_eq!(out, "42\n");
}

#[test]
fn preempt_range_for_resumes_with_frame_backed_index() {
    let source = r#"
async fn sum() -> i64 {
    let mut total = 0;
    for i in 0..100 {
        total = total + i;
    }
    return total;
}
async fn main() {
    println(await sum());
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(ok, "range-for should resume after every preemption");
    assert_eq!(out, "4950\n");
}

#[test]
fn preempt_array_for_keeps_gc_values_live() {
    let source = r#"
import std::collections::Array;
async fn concatenate() -> String {
    let values: Array<String> = ["a", "b", "c"];
    let mut out = "";
    for value in values {
        out = out + value;
    }
    return out;
}
async fn main() {
    println(await concatenate());
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "array-for GC values must survive preemption");
    assert_eq!(out, "abc\n");
}

#[test]
fn preempt_call_boundaries_interleave_straight_line_tasks() {
    let source = r#"
async fn first() -> i64 {
    println(1);
    println(3);
    return 0;
}
async fn second() -> i64 {
    println(2);
    return 0;
}
async fn main() {
    let a = first();
    let b = second();
    await a;
    await b;
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(
        ok,
        "straight-line calls should resume after call-site safepoints"
    );
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    for expected in ["1", "2", "3"] {
        assert!(lines.contains(&expected), "{out}");
    }
    assert!(
        lines.iter().position(|line| *line == "1") < lines.iter().position(|line| *line == "3"),
        "first task must preserve its own statement order: {out}"
    );
}

#[test]
fn preempt_await_channel_and_allocation_statements_resume() {
    let source = r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    let value = "pre" + "empt";
    ch.send(value);
    return 0;
}
async fn consumer(ch: Channel<String>) -> String {
    let value = ch.recv();
    return value;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c);
    await p;
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "await/channel/allocation statements must resume safely");
    assert_eq!(out, "preempt\n");
}
