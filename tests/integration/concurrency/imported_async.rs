use super::*;

// ---------------------------------------------------------------------------
// Cross-module async fn call types as `Task<T>` at the call site (willow-887c).
// A module-qualified call to an async fn must yield a task so `await`
// type-checks, exactly like a local async call.
// ---------------------------------------------------------------------------

#[test]
fn test_cross_module_async_fn_await_of_immediate_value() {
    let worker = r#"
pub async fn make_value() -> i64 {
    return 42;
}
"#;
    let main = r#"
import worker;

async fn main() {
    println(await worker::make_value());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(ok, "cross-module `await` should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_cross_module_async_fn_await_returns_value() {
    let worker = r#"
pub async fn make_value() -> i64 {
    await sleep(1);
    return 42;
}
"#;
    let main = r#"
import worker;

async fn main() {
    println(await worker::make_value());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "cross-module `await` of an async fn should compile and run"
    );
    assert_eq!(out, "42\n");
}

#[test]
fn test_item_imported_async_fn_await_returns_value() {
    // Item-imported async fn called by its bare local name already wraps to
    // `Task<T>`; guard against regressing that alongside the module-qualified fix.
    let worker = r#"
pub async fn make_value() -> i64 {
    return 42;
}
"#;
    let main = r#"
import worker::make_value;

async fn main() {
    println(await make_value());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(ok, "item-imported async fn await should compile and run");
    assert_eq!(out, "42\n");
}

// ---------------------------------------------------------------------------
// Awaiting a NON-leaf (item-imported) async fn from a cooperative poll fn must
// suspend cooperatively, not block-drive the scheduler (willow-0a6k.6). Output
// correctness here also guards the frame-slot reload on resume: without a
// reserved callee-frame slot the resume path would re-run the call.
// ---------------------------------------------------------------------------

#[test]
fn test_await_item_imported_async_from_cooperative_fn_runs_once() {
    let worker = r#"
pub async fn make_value() -> i64 {
    await sleep(1);
    println(99);
    return 42;
}
"#;
    let main = r#"
import worker::make_value;

async fn run() -> i64 {
    let x = await make_value();
    return x;
}

async fn main() {
    println(await run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "awaiting an item-imported async fn should compile and run"
    );
    // `99` printed exactly once (single call), then the awaited result.
    assert_eq!(out, "99\n42\n");
}

#[test]
fn test_await_item_imported_async_in_loop_reuses_slot() {
    let worker = r#"
pub async fn inc(n: i64) -> i64 {
    await sleep(1);
    return n + 1;
}
"#;
    let main = r#"
import worker::inc;

async fn run() -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        total = total + await inc(i);
        i = i + 1;
    }
    return total;
}

async fn main() {
    println(await run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "awaiting an item-imported async fn in a loop should run"
    );
    // inc(0)+inc(1)+inc(2) = 1+2+3 = 6, with the suspend/resume slot reused each
    // iteration.
    assert_eq!(out, "6\n");
}
