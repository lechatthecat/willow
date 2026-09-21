use super::*;

// ----------------------------------------------------------------------------
// Async-GC stress suite (willow-lpn.5.5): GC-safety of the cooperative state
// machine — collection before await, after await, GC objects/strings carried
// across awaits, and JoinHandle keeping a GC result alive. All under
// WILLOW_GC_STRESS=alloc (collect at every allocation) plus explicit gc_collect.
// ----------------------------------------------------------------------------

// 16.1: collection BEFORE an await — a frame-backed GC local survives.
#[test]
fn coop_gc_01_collect_before_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> String {
    let s = "kept";
    gc_collect();
    await sleep(1);
    return s;
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}

// 16.2: collection AFTER an await — the local declared before the await survives.
#[test]
fn coop_gc_02_collect_after_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> String {
    let s = "kept";
    await sleep(1);
    gc_collect();
    return s;
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}

// GC object (class instance) carried across an await with collections on both
// sides; field access after the await reads the live object.
#[test]
fn coop_gc_03_object_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { v: i64; pub static fn new(v: i64) -> Box { return new Box(v); } pub fn get(self) -> i64 { return self.v; } }
async fn run() -> i64 {
    let b = Box::new(42);
    gc_collect();
    await sleep(1);
    gc_collect();
    return b.get();
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

// 16.9: a JoinHandle keeps the task's GC result alive across a collection
// performed before `await`.
#[test]
fn coop_gc_04_joinhandle_keeps_result_alive() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn tag(n: i64) -> String { return "tag"; }
async fn main() {
    let h = tag(7);
    gc_collect();
    gc_collect();
    println(await h);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "tag\n");
}

// Combined stress: many awaits in a loop, each iteration allocates (string
// concat) and collects, while the accumulator local survives every collection.
#[test]
fn coop_gc_05_combined_stress_loop() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn build(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        await sleep(1);
        s = s + "ab";
        gc_collect();
        i = i + 1;
    }
    return s;
}
async fn main() { println(await build(4)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "abababab\n");
}

// Awaiting a cooperative-leaf async fn must return the async function's
// REAL result, not the constructor's frame pointer (willow-lpn.5.4 fix).
#[test]
fn coop_spawn_06_spawn_async_leaf_sync_main() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn main() {
    let h = work(41);
    println(await h);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_spawn_07_spawn_async_leaf_multiple_gc() {
    // Multiple spawned async leaves (i64 + String results) awaited; under GC
    // stress to exercise frame/result tracing.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn add(a: i64, b: i64) -> i64 {
    await sleep(1);
    return a + b;
}
async fn tag(name: String) -> String {
    await sleep(1);
    return "hi " + name;
}
async fn main() {
    let h1 = add(40, 2);
    let h2 = add(10, 5);
    let h3 = tag("willow");
    println(await h1);
    println(await h2);
    println(await h3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n15\nhi willow\n");
}

#[test]
fn coop_spawn_08_spawn_async_leaf_runs_to_completion() {
    // The spawned leaf actually runs (side effects observed), spawn does not
    // block the spawner, and await returns the leaf's real result.
    //
    // The exact interleaving of the spawner's prints with the leaf's is NOT an
    // invariant and must not be asserted (willow-0uce). `willow_sched_spawn`
    // publishes the task to the shared run queues and the runtime drives them
    // on the configured worker pool. With multiple workers, a peer worker
    // may claim and poll `work` the instant it is spawned,
    // concurrently with `main` running on to `println(2)`. Both `1 2 100 ...`
    // and `1 100 2 ...` are legal; a wider gap between the spawn and the next
    // statement makes `1 100 200 2 ...` legal too. Asserting one ordering was
    // a ~10% flake.
    //
    // What IS guaranteed, and is what this test exists to check:
    //   * every side effect happens exactly once,
    //   * each task's own prints keep their program order (1 before 2 before 3
    //     in `main`, 100 before 200 in `work`),
    //   * `await h` does not return until `work` has run to completion, so 200
    //     precedes 3, and
    //   * the awaited value is the leaf's real return value.
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    println(100);
    await sleep(1);
    println(200);
    return x;
}
async fn main() {
    println(1);
    let h = work(42);
    println(2);
    let r = await h;
    println(3);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");

    let lines: Vec<&str> = out.lines().collect();
    let at = |needle: &str| {
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == needle)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "{needle} must be printed exactly once: {out}"
        );
        hits[0]
    };
    assert_eq!(lines.len(), 6, "unexpected extra/missing output: {out}");
    let (one, two, three, result) = (at("1"), at("2"), at("3"), at("42"));
    let (leaf_start, leaf_end) = (at("100"), at("200"));
    assert!(one < two && two < three, "spawner order broken: {out}");
    assert!(leaf_start < leaf_end, "leaf order broken: {out}");
    assert!(one < leaf_start, "leaf ran before it was spawned: {out}");
    assert!(
        leaf_end < three,
        "await returned before the leaf finished: {out}"
    );
    assert_eq!(result, three + 1, "awaited value must be the leaf's: {out}");
}

// Cooperative concurrency: spawned async-leaf tasks suspend independently at
// their awaits and the scheduler interleaves them — observably distinct from
// sequential execution (willow-lpn.5.4). The scheduler is a worker POOL, not a
// single thread, so only each task's own output order is an invariant; the
// interleaving between tasks is not (willow-0uce).
#[test]
fn coop_concurrent_01_two_workers_interleave() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await sleep(1);
    println(id + 100);
    return id;
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
        lines[4], "3",
        "both awaits must complete before the sum: {out}"
    );
    for (start, finish) in [("1", "101"), ("2", "102")] {
        let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} reordered its output: {out}"
        );
    }
}

#[test]
fn coop_yield_01_main_resumes_without_timer() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    println(1);
    await yield();
    println(2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn coop_yield_02_spawned_workers_interleave() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await yield();
    println(id + 10);
    return id;
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
        lines[4], "3",
        "both awaits must complete before the sum: {out}"
    );
    for (start, finish) in [("1", "11"), ("2", "12")] {
        let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} reordered its output: {out}"
        );
    }
}

#[test]
fn coop_yield_03_gc_string_survives_yield() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn keep(text: String) -> String {
    let held = text + "!";
    gc_collect();
    await yield();
    gc_collect();
    return held + "?";
}
async fn main() {
    let task = keep("yield");
    gc_collect();
    println(await task);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "yield!?\n");
}

#[test]
fn coop_concurrent_02_three_workers_interleave_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await sleep(1);
    println(id * 10);
    return id;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    let c = worker(3);
    println(await a + await b + await c);
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 7, "{out}");
    assert_eq!(lines[6], "6", "sum must print after every await: {out}");
    for (start, finish) in [("1", "10"), ("2", "20"), ("3", "30")] {
        let start_at = lines[..6].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..6].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} finished before it started: {out}"
        );
    }
}

#[test]
fn coop_concurrent_03_spawn_then_await_in_main() {
    // An eager main spawns a background worker, then `await f()` block-drives the
    // scheduler — the background worker interleaves during that await.
    let (out, ok) = compile_and_run(
        r#"
async fn bg() -> i64 {
    println(7);
    await sleep(1);
    println(8);
    return 0;
}
async fn f() -> i64 {
    await sleep(1);
    return 42;
}
async fn main() {
    let h = bg();
    let x = await f();
    println(x);
    await h;
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{out}");
    for value in ["7", "8", "42"] {
        assert!(lines.contains(&value), "missing {value}: {out}");
    }
    let started = lines.iter().position(|line| *line == "7").unwrap();
    let finished = lines.iter().position(|line| *line == "8").unwrap();
    assert!(
        started < finished,
        "background task reordered its output: {out}"
    );
}
