use super::*;

// ----------------------------------------------------------------------------
// Cooperative awaiter-suspend model (willow-lpn.5.3.1): a `let x = await f()` /
// `await f()` of a cooperative leaf SUSPENDS the awaiter via willow_sched_await
// (dependency-wake) rather than block-on, so a fn that MIXES call-awaits and
// sleep-awaits is itself a cooperative task. The callee frame is held in a
// GC-traced awaiter slot across suspension.
// ----------------------------------------------------------------------------

// A spawned worker that mixes a call-await and a sleep-await returns its REAL
// result (previously returned a frame ptr / garbage).
#[test]
fn coop_await_01_mixed_call_and_sleep_await_spawned() {
    let (out, ok) = compile_and_run(
        r#"
async fn helper(x: i64) -> i64 {
    await sleep(1);
    return x * 10;
}
async fn worker(id: i64) -> i64 {
    println(id);
    let h = await helper(id);
    await sleep(1);
    println(h);
    return h + id;
}
async fn main() {
    let a = worker(1);
    println(await a);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n10\n11\n");
}

// Two mixed-await workers interleave (true concurrency WITH composition), GC.
#[test]
fn coop_await_02_mixed_workers_interleave_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn helper(x: i64) -> i64 {
    await sleep(1);
    return x * 10;
}
async fn worker(id: i64) -> i64 {
    println(id);
    let h = await helper(id);
    println(h);
    return h + id;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    println(await a + await b);
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(
        lines[4], "33",
        "both awaits must complete before the sum: {out}"
    );
    for (start, finish) in [("1", "10"), ("2", "20")] {
        let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} reordered its output: {out}"
        );
    }
}

// Sequential call-awaits chaining a GC (String) result through the awaiter
// frame, under GC stress.
#[test]
fn coop_await_03_sequential_string_call_awaits_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn step(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let a = await step("a");
    let b = await step(a);
    let c = await step(b);
    println(c);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!!!\n");
}

// A call-await result drives later control flow + arithmetic in the awaiter.
#[test]
fn coop_await_04_call_await_result_in_control_flow() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn compute(x: i64) -> i64 {
    await sleep(1);
    return x + 5;
}
async fn main() {
    let v = await compute(10);
    if v > 12 {
        await sleep(1);
        println(v * 2);
    } else {
        println(0);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "30\n");
}

// A discarded call-await (`await f();` with no binding) still suspends + runs.
#[test]
fn coop_await_05_discarded_call_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn tick(n: i64) -> i64 {
    await sleep(1);
    println(n);
    return n;
}
async fn main() {
    await tick(1);
    await tick(2);
    println(3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// A call-await can assign into an existing frame-backed local and then keep
// running after another suspension.
#[test]
fn coop_await_06_assignment_call_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn next(n: i64) -> i64 {
    await sleep(1);
    return n + 1;
}
async fn worker() -> i64 {
    let mut total = 0;
    total = await next(10);
    await sleep(1);
    return total + 5;
}
async fn main() {
    println(await worker());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n");
}

// A cooperative leaf can return the result of a call-await directly.
#[test]
fn coop_await_07_return_call_await_chain_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn wrap(s: String) -> String {
    return await mark(s);
}
async fn main() {
    println(await wrap("ok"));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ok!\n");
}

// A call-await can assign a GC result into an object field, then survive another
// suspension before the field is read.
#[test]
fn coop_await_08_field_assignment_call_await_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Holder {
    pub text: String;
}
async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let h = new Holder("seed");
    h.text = await mark("field");
    await sleep(1);
    println(h.text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "field!\n");
}

// A call-await can assign a GC result into an array element through the
// cooperative awaiter path.
#[test]
fn coop_await_09_index_assignment_call_await_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let mut xs: Array<String> = ["seed"];
    xs[0] = await mark("index");
    await sleep(1);
    println(xs[0]);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "index!\n");
}
