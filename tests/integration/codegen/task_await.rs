use super::*;

// ----------------------------------------------------------------------------
// Cooperative task await (willow: async work migrated off one-OS-thread-per
// task onto the cooperative scheduler). Calling an async fn queues a
// lightweight task; `await` suspends until its target completes.
// ----------------------------------------------------------------------------

// Await returns each task's result, regardless of await order.
#[test]
fn coop_spawn_01_await_order_independent() {
    let (out, ok) = compile_and_run(
        r#"
async fn sq(x: i64) -> i64 { return x * x; }
async fn main() {
    let a = sq(2);
    let b = sq(3);
    let c = sq(4);
    println(await c);
    println(await a);
    println(await b);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n4\n9\n");
}

// Many lightweight tasks: spawning a lot is cheap (no OS thread per spawn).
#[test]
fn coop_spawn_02_many_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn id(x: i64) -> i64 { return x; }
async fn main() {
    let a = id(1);
    let b = id(2);
    let c = id(3);
    let d = id(4);
    let e = id(5);
    let f = id(6);
    let g = id(7);
    let h = id(8);
    let total = await a + await b + await c + await d
        + await e + await f + await g + await h;
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "36\n");
}

// A spawned producer is driven by the consumer's `recv()` (cooperative, no
// cross-thread deadlock).
#[test]
fn coop_spawn_03_channel_producer_consumer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(1);
    ch.send(2);
    ch.send(3);
    ch.close();
}
async fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    println(ch.recv());
    println(ch.recv());
    println(ch.recv());
    await h;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Task with GC-managed args (object + string), result read via await, under
// GC stress: the frame roots the args and traces the result slot.
#[test]
fn coop_spawn_04_gc_args_and_result() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { v: i64; pub static fn new(v: i64) -> Box { return new Box(v); } pub fn get(self) -> i64 { return self.v; } }
async fn label(b: Box, name: String) -> String {
    return name;
}
async fn value(b: Box) -> i64 {
    return b.get();
}
async fn main() {
    let b = Box::new(7);
    let h1 = label(b, "tag");
    let h2 = value(b);
    println(await h1);
    println(await h2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "tag\n7\n");
}

// A non-i64 (bool) spawn result round-trips through the frame result slot.
#[test]
fn coop_spawn_05_bool_result() {
    let (out, ok) = compile_and_run(
        r#"
async fn positive(x: i64) -> bool { return x > 0; }
async fn main() {
    let a = positive(5);
    let b = positive(-5);
    println(await a);
    println(await b);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

// Slice 5: awaits inside if/else and while are lowered by the CFG-based
// cooperative state machine (willow-lpn.5.3 / willow-8fh3 regression).
#[test]
fn coop_async_09_await_in_if_else_both_return() {
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
    println(await pick(true));
    println(await pick(false));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn coop_async_10_await_in_if_else_merge() {
    // Both arms fall through to a shared CFG merge, carrying a frame-backed local.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run(flag: bool) -> i64 {
    let mut r = 0;
    if flag {
        await sleep(1);
        r = 10;
    } else {
        await sleep(1);
        r = 20;
    }
    await sleep(1);
    return r + 1;
}
async fn main() {
    println(await run(true));
    println(await run(false));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "11\n21\n");
}

#[test]
fn coop_async_11_await_in_while() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sum(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        await sleep(1);
        total = total + i;
        i = i + 1;
    }
    return total;
}
async fn main() { println(await sum(4)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn coop_async_12_await_in_if_inside_while() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        if i == 1 {
            await sleep(1);
            total = total + 100;
        } else {
            await sleep(1);
            total = total + i;
        }
        i = i + 1;
    }
    return total;
}
async fn main() { println(await run(3)); }
"#,
    );
    assert!(ok, "{out}");
    // i=0: +0, i=1: +100, i=2: +2 => 102
    assert_eq!(out, "102\n");
}

#[test]
fn coop_async_13_gc_string_built_across_while_awaits() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn build(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        await sleep(1);
        s = s + "x";
        i = i + 1;
    }
    return s;
}
async fn main() { println(await build(3)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xxx\n");
}
