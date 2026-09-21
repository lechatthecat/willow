use super::*;

// ── Async frame: frame-backed GC params survive across await (willow-lpn.5a) ──

#[test]
fn async_frame_01_string_param_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn echo(s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await echo("hello"));
}
"#,
    );
    assert!(ok, "async String param across await must work: {out}");
    assert_eq!(out, "hello\n");
}

#[test]
fn async_frame_02_second_param_slot_indexing() {
    // Returning the second GC param verifies per-slot frame offsets.
    let (out, ok) = compile_and_run(
        r#"
async fn pick(a: String, b: String) -> String {
    await sleep(1);
    return b;
}
async fn main() {
    println(await pick("first", "second"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "second\n");
}

#[test]
fn async_frame_03_mixed_gc_and_scalar_params() {
    // A non-GC param (slot 0) stays on the stack; the GC param (slot 1) is
    // frame-backed — exercises slot-indexed offsets independent of which slots
    // are frame-backed.
    let (out, ok) = compile_and_run(
        r#"
async fn pick(n: i64, s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await pick(7, "kept"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "kept\n");
}

#[test]
fn async_frame_04_gc_stress_param_survives() {
    // The String param is reachable only through the heap frame across the
    // await; it must survive collection at every allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn echo(s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await echo("hello world"));
}
"#,
    );
    assert!(ok, "frame-backed param must survive GC stress: {out}");
    assert_eq!(out, "hello world\n");
}

#[test]
fn async_frame_05_annotated_string_local_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn make() -> String {
    let s: String = "local value";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "annotated GC local across await must work: {out}");
    assert_eq!(out, "local value\n");
}

#[test]
fn async_frame_06_mutated_frame_local_round_trips() {
    // The local is read+written on both sides of the await; values must round
    // trip through the heap frame slot.
    let (out, ok) = compile_and_run(
        r#"
async fn build() -> String {
    let mut s: String = "a";
    s = s + "b";
    await sleep(1);
    s = s + "c";
    return s;
}
async fn main() {
    println(await build());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "abc\n");
}

#[test]
fn async_frame_07_gc_stress_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make() -> String {
    let s: String = "kept across await";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "frame-backed local must survive GC stress: {out}");
    assert_eq!(out, "kept across await\n");
}

// ── lpn.5c slice 1: unannotated locals frame-backed via type-checker types ──

#[test]
fn async_frame_08_unannotated_local_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn make() -> String {
    let s = "unannotated";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "unannotated GC local across await must work: {out}");
    assert_eq!(out, "unannotated\n");
}

#[test]
fn async_frame_09_unannotated_local_mutated_round_trips() {
    let (out, ok) = compile_and_run(
        r#"
async fn build() -> String {
    let mut s = "x";
    await sleep(1);
    s = s + "y";
    return s;
}
async fn main() {
    println(await build());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "xy\n");
}

#[test]
fn async_frame_10_unannotated_local_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make() -> String {
    let s = "inferred kept";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(
        ok,
        "unannotated frame-backed local must survive GC stress: {out}"
    );
    assert_eq!(out, "inferred kept\n");
}

// ── Frame-backed values across await: GC tracing by type (lpn.5c perspectives) ──
// Each value lives ONLY in the GC-rooted heap frame across the await, so these
// verify the frame's per-type GC tracing under collection at every allocation.

#[test]
fn async_frame_11_class_with_ref_field_survives() {
    // Two-level tracing: frame traces the Box, Box's mask traces its String field.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { pub s: String; }
async fn f() -> String {
    let b: Box = new Box("nested");
    await sleep(1);
    return b.s;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "class with ref field must survive across await: {out}");
    assert_eq!(out, "nested\n");
}

#[test]
fn async_frame_12_array_of_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn f() -> String {
    let xs: Array<String> = [];
    xs.push("e0");
    xs.push("e1");
    await sleep(1);
    return xs[1];
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Array<String> must survive across await: {out}");
    assert_eq!(out, "e1\n");
}

#[test]
fn async_frame_13_option_payload_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let o: Option<String> = Option::Some("opt");
    await sleep(1);
    return match o { Option::Some(x) => x, Option::None => "none", };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Option<String> payload must survive across await: {out}"
    );
    assert_eq!(out, "opt\n");
}

#[test]
fn async_frame_14_result_payload_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let r: Result<String, String> = Result::Ok("ok");
    await sleep(1);
    return match r { Result::Ok(x) => x, Result::Err(e) => e, };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Result payload must survive across await: {out}");
    assert_eq!(out, "ok\n");
}

#[test]
fn async_frame_15_map_ref_value_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Map;

async fn f() -> String {
    let mut m: Map<String, String> = Map::new();
    m.insert("k", "val");
    await sleep(1);
    return match m.get("k") { Option::Some(v) => v, Option::None => "missing", };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Map ref value must survive across await: {out}");
    assert_eq!(out, "val\n");
}

#[test]
fn async_frame_16_option_some_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { pub value: i64; pub next: Option<Node>; }
async fn f(n: Option<Node>) -> i64 {
    await sleep(1);
    return match n { Some(value) => value.value, None => -1 };
}
async fn main() { println(await f(Some(new Node(77, None)))); }
"#,
    );
    assert!(ok, "Some payload must survive across await: {out}");
    assert_eq!(out, "77\n");
}

#[test]
fn async_frame_17_option_none_traced_as_zero_niche() {
    // A niche None in a GC frame slot must be skipped (not dereferenced) by the
    // collector, not crash.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { pub value: i64; pub next: Option<Node>; }
async fn f(n: Option<Node>) -> i64 {
    await sleep(1);
    return match n { Some(value) => value.value, None => -1 };
}
async fn main() { println(await f(None)); }
"#,
    );
    assert!(
        ok,
        "None Option frame slot must be safe across await: {out}"
    );
    assert_eq!(out, "-1\n");
}

#[test]
fn async_frame_18_task_local_traced_across_await() {
    // A Task local held across an await is a GC async-frame pointer; it must be
    // traced as a heap object and remain awaitable after collection.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn other() -> i64 { return 7; }
async fn f() -> i64 {
    let fut = other();
    await sleep(1);
    return await fut;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Task local across await must stay alive across collection: {out}"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn async_frame_19_join_handle_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn work() { println("worked"); }
async fn f() {
    let h = work();
    await sleep(1);
    await h;
}
async fn main() { await f(); }
"#,
    );
    assert!(
        ok,
        "JoinHandle local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "worked\n");
}

#[test]
fn async_frame_20_channel_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) { ch.send(11); ch.close(); }
async fn f() -> i64 {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    await sleep(1);
    let v = ch.recv();
    await h;
    return v;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Channel local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "11\n");
}
