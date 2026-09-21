use super::*;

// ── Cooperative async suspension (willow-lpn.5.3 Stage 2) ────────────────────

#[test]
fn coop_async_01_main_suspends_at_sleep() {
    // An eligible `async fn main` lowers to a suspending poll-fn state machine
    // driven by the scheduler; output is produced across the await points.
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    println(1);
    await sleep(1);
    println(2);
    await sleep(1);
    println(3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

#[test]
fn coop_async_02_no_await_before_first_output() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    await sleep(1);
    println(42);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_async_03_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    println(1);
    await sleep(1);
    println(2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn coop_async_04_gc_locals_across_awaits() {
    // GC-managed locals declared before an await and used after must survive
    // suspension (frame-backed). Run under GC stress (willow-lpn.5.3 slice 3).
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let s = "hello";
    await sleep(1);
    println(s);
    let t = s + " world";
    await sleep(1);
    println(t);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello\nhello world\n");
}

#[test]
fn coop_async_05_non_gc_locals_across_awaits() {
    // i64/scalar locals across awaits are frame-backed too (not just GC), and
    // are not GC-traced (willow-lpn.5.3 slice 3b). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let n = 10;
    let s = "v=";
    await sleep(1);
    let m = n + 5;
    println(s);
    println(m);
    await sleep(1);
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "v=\n15\n10\n");
}

#[test]
fn coop_async_06_await_cooperative_leaf() {
    // A no-param leaf async fn (sleep + return) compiles to a cooperative
    // constructor + poll fn; `await f()` block-runs the scheduler and reads the
    // result (willow-lpn.5.3 slice 4). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn wait_value() -> i64 {
    await sleep(1);
    return 42;
}
async fn compute() -> i64 {
    await sleep(1);
    return 7;
}
async fn main() {
    let x = await wait_value();
    println(x);
    let y = await compute();
    println(y + 1);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n8\n");
}

#[test]
fn coop_async_06b_eager_await_roots_leaf_frame_until_result_load() {
    // `println(await f())` is intentionally not eligible for the cooperative
    // awaiter lowering, so it exercises the eager emit_await() path. That path
    // must keep the completed leaf frame rooted while willow_sched_run() removes
    // the task runtime root and before frame[RESULT] is loaded.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make_text() -> String {
    await sleep(1);
    return "root" + "ed";
}
async fn make_number() -> i64 {
    await sleep(1);
    return 42;
}
async fn main() {
    println(await make_text());
    println(await make_number());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rooted\n42\n");
}

#[test]
fn coop_async_06c_eager_await_survives_await_stress() {
    let (out, ok) = compile_and_run_gc_stress_mode(
        r#"
async fn make_text() -> String {
    await sleep(1);
    return "await" + "-stress";
}
async fn main() {
    println(await make_text());
}
"#,
        "await",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "await-stress\n");
}

#[test]
fn coop_async_07_cooperative_leaf_with_params() {
    // A leaf async fn with by-value params (GC + scalar) compiles to a
    // cooperative constructor that stores args into frame slots; the poll fn
    // reads them back across the suspension (willow-lpn.5.3 slice 4b). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn greet(name: String, n: i64) -> String {
    await sleep(1);
    return "hi " + name;
}
async fn add(a: i64, b: i64) -> i64 {
    await sleep(1);
    return a + b;
}
async fn main() {
    let g = await greet("willow", 3);
    println(g);
    let s = await add(40, 2);
    println(s);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi willow\n42\n");
}

#[test]
fn coop_async_08_cooperative_leaf_with_locals() {
    // A cooperative leaf may declare locals (GC + scalar) that survive its own
    // suspensions, frame-backed after the param slots (willow-lpn.5.3 4c).
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn calc(base: i64) -> i64 {
    let a = base + 1;
    let label = "result";
    await sleep(1);
    let b = a * 2;
    await sleep(1);
    println(label);
    return b + base;
}
async fn main() {
    let r = await calc(10);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "result\n32\n");
}

#[test]
fn coop_async_09_await_inside_if_and_while_in_main() {
    // Slice 5: structured control flow in the cooperative main poll fn, including
    // a loop back-edge and branch-local suspend points.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        if i == 1 {
            await sleep(1);
            println(10);
        } else {
            await sleep(1);
            println(i);
        }
        i = i + 1;
    }
    println(99);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n10\n2\n99\n");
}

#[test]
fn coop_async_10_await_inside_leaf_if_else_returns() {
    // Slice 5 regression: both branches can suspend and then return from a
    // cooperative leaf poll fn.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn pick(flag: bool) -> i64 {
    if flag {
        await sleep(1);
        return 10;
    } else {
        await sleep(1);
        await sleep(1);
        return 20;
    }
}
async fn main() {
    let a = await pick(true);
    println(a);
    let b = await pick(false);
    println(b);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn coop_async_11_await_inside_for_loop_in_main() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn main() {
    let xs: Array<i64> = [1, 2, 3];
    for x in xs {
        await sleep(1);
        println(x);
    }
    println(99);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n99\n");
}

#[test]
fn coop_async_12_await_inside_for_loop_in_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn sum(values: FrozenArray<i64>) -> i64 {
    let mut total = 0;
    let mut index = 0;
    while index < values.len() {
        await sleep(1);
        total = total + values[index];
        index = index + 1;
    }
    return total;
}

async fn main() {
    let values: Array<i64> = [4, 5, 6];
    let total = await sum(values.freeze());
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}

#[test]
fn iface_same_named_modules_keep_distinct_vtables() {
    let one = r#"
module one;
pub interface View { fn value(self) -> i64; }
pub class First implements View {
    pub fn value(self) -> i64 { return 11; }
}
pub fn read(v: View) -> i64 { return v.value(); }
"#;
    let two = r#"
module two;
pub interface View { fn value(self) -> i64; }
pub class Second implements View {
    pub fn value(self) -> i64 { return 22; }
}
pub fn read(v: View) -> i64 { return v.value(); }
"#;
    let main = r#"
import one;
import two;
fn main() {
    let first: one::View = new one::First();
    let second: two::View = new two::Second();
    println(first.value());
    println(second.value());
    println(one::read(new one::First()));
    println(two::read(new two::Second()));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[("one.wi", one), ("two.wi", two), ("main.wi", main)],
        "main.wi",
    );
    assert!(ok, "same-named interface dispatch failed: {out}");
    assert_eq!(out, "11\n22\n11\n22\n");
}

#[test]
fn gc_survivor_age_two_and_short_lived_batch() {
    let source = r#"
import std::collections::Array;
class Payload { pub value: i64; }
fn fill(values: Array<Payload>) {
    for i in 0..32 { values.push(new Payload(i)); }
}
fn drain(values: Array<Payload>) {
    while values.len() > 0 { values.pop(); }
}
fn main() {
    let values: Array<Payload> = [];
    fill(values);
    let promoted = gc_promoted_objects();
    let copied = gc_survivor_copies();
    gc_minor_collect();
    println(gc_survivor_copies() - copied);
    println(gc_promoted_objects() - promoted);
    println(gc_survivor_space_live() > 0);
    gc_minor_collect();
    println(gc_tenured_objects());
    println(values[31].value);
    let temporary: Array<Payload> = [];
    fill(temporary);
    // Discard obsolete array buffers from growth before measuring lifetime.
    gc_collect();
    let before = gc_promoted_objects();
    gc_minor_collect();
    drain(temporary);
    gc_minor_collect();
    println(gc_promoted_objects() - before);
    println(gc_survivor_space_live());
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[]);
    assert!(ok, "survivor program failed: {out}");
    assert_eq!(out, "32\n0\ntrue\n32\n31\n0\n0\n");
}
