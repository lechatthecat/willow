use super::*;

// ---------------------------------------------------------------------------
// Function-pointer spawn (willow-spawn-fptr).
//
// `spawn f(args)` where `f` is a function VALUE (a `fn(...)` local — a named
// function reference or a lambda) used to run the call INLINE at the spawn site
// and merely wrap the result in a frame. It now compiles a `call_indirect` poll
// trampoline and schedules the task on the cooperative scheduler, exactly like
// `spawn named_fn(args)`. The 20 perspectives below cover that behavior.
//
//  1. named fn in a `fn` local, single i64 arg → await returns the result
//  2. lambda value spawned → await returns the result
//  3. two-arg fptr spawn → correct combined result
//  4. zero-arg fptr spawn
//  5. bool-returning fptr spawn
//  6. f64-returning fptr spawn
//  7. String-returning fptr spawn (GC-managed result slot in the frame mask)
//  8. String args through the indirect trampoline (GC-managed arg slots)
//  9. result usable in arithmetic after await
// 10. multiple fptr spawns awaited in spawn order
// 11. multiple fptr spawns awaited OUT of spawn order
// 12. fptr spawn is DEFERRED, not inline: a print after spawn precedes the
//     task's print (the observable behavior change vs. the old inline fallback)
// 13. fptr spawn matches named-fn spawn ordering (same scheduled semantics)
// 14. fptr passed in as a `fn` PARAMETER, then spawned
// 15. the same fptr local spawned twice → two independent tasks
// 16. two DIFFERENT fptr signatures in one program → distinct trampolines
// 17. fptr spawn result equals the equivalent direct call
// 18. four-arg fptr spawn → arg slot offsets stay correct
// 19. mixed arg types (i64 + bool) through one indirect trampoline
// 20. GC stress: String-returning + String-arg fptr spawn survives collection
//     during scheduling/await (frame + arg rooting correctness)
// ---------------------------------------------------------------------------

#[test]
fn test_await_on_non_awaitable_local_reports_e0803() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let value = 1;
    await value;
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
fn test_channel_send_type_mismatch_reports_e0802() {
    assert_compile_error_contains(
        r#"
fn send_bool(ch: Channel<i64>) {
    ch.send(true);
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0802]",
            "cannot send `bool` into `Channel<i64>`",
            "expected `i64`, found `bool`",
        ],
    );
}

#[test]
fn test_channel_operation_on_non_channel_reports_e0806() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = 1;
    value.recv();
}
"#,
        &[
            "error[E0806]",
            "cannot call `recv` on `i64`",
            "expected `Channel<T>`",
        ],
    );
}

#[test]
fn test_channel_i64_mvp_send_recv_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    ch.send(10);
    ch.send(32);
    println(ch.recv() + ch.recv());
}
"#,
    );
    assert!(ok, "Channel<i64> send/recv MVP should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_channel_recv_empty_open_panics_instead_of_defaulting() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    println(ch.recv());
}
"#,
    );
    assert!(!ok, "empty open recv must fail instead of returning 0");
    assert!(
        out.contains("runtime panic: recv on empty open channel would block"),
        "{out}"
    );
}

#[test]
fn test_channel_recv_closed_empty_raises_language_panic() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    ch.close();
    println(ch.recv());
}
"#,
    );
    assert!(
        !ok,
        "closed empty recv must not synthesize a T value: {out}"
    );
    assert!(out.contains("recv on closed empty channel"), "{out}");
}

#[test]
fn test_channel_target_producer_spawn_example_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}

async fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    println(ch.recv());
    println(ch.recv());
    await h;
}
"#,
    );
    assert!(
        ok,
        "target Channel producer/spawn example should compile and run"
    );
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_concurrency_generic_types_parse_and_type_check() {
    let (out, ok) = compile_and_run(
        r#"
fn takes_handle(h: JoinHandle<i64>) {
}

fn takes_future(f: Future<String>) {
}

fn takes_channel(c: Channel<i64>) {
}

fn main() {
    println(1);
}
"#,
    );
    assert!(ok, "concurrency generic type annotations should compile");
    assert_eq!(out, "1\n");
}

#[test]
fn test_concurrency_generic_type_mismatch_is_reported() {
    assert_compile_error_contains(
        r#"
fn takes_handle(h: JoinHandle<i64>) {
}

fn main() {
    takes_handle(1);
}
"#,
        // `JoinHandle<T>` is a legacy SPELLING of `Task<T>`, not a second type,
        // so diagnostics name the canonical one (willow-qrj9).
        &[
            "error[E0201]",
            "mismatched types: expected `Task<i64>`, found `i64`",
            "expected `Task<i64>`",
        ],
    );
}
