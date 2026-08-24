//! `parallel::map` compiled from Lowered IR (willow-0g8j.2.13).
//!
//! `parallel` is a NAMESPACE, not a class: it has no layout, no methods and no
//! symbols of its own, so `parallel::map(frozen, f)` is one direct runtime call
//! on both backends. The walker keeps a single table of those namespace entries
//! shared by eligibility and emission — so it cannot admit a call it has no
//! entry point for — and takes the SIGNATURE from the stdlib schema, the same
//! table the checker types the call from.
//!
//! What kept this entry out until now is its arguments, which are the only ones
//! in that table that are neither scalars nor strings:
//!
//!   * a `FrozenArray<i64>`, a heap handle, rooted across the call like any
//!     other — several perspectives here allocate under `WILLOW_GC_STRESS=alloc`
//!     while the handle is live, where an unrooted slot is collected rather than
//!     merely at risk;
//!   * a FUNCTION VALUE, which is deliberately NOT rooted: `parallel::map`
//!     rejects a capturing lambda at type-check time, so a mapper is a bare code
//!     address with nothing for the collector to trace.
//!
//! The language contract — ordering, cancellation, panic policy, the rejections
//! — is pinned by the `parallel_map_*` tests in `concurrency.rs` on the default
//! backend. These tests pin the WALKER: every one asserts the same output from
//! the AST emitter and the walker, and confirms the walker is the path that ran,
//! so a coverage regression cannot pass vacuously by comparing the AST path
//! against itself.
//!
//! 20 perspectives:
//!   1 named mapper keeps source order   11 cancellation has no partial result
//!   2 a lambda is a mapper too          12 a `TaskScope` owns the task
//!   3 empty and singleton inputs        13 the handle lives in a class field
//!   4 a sync helper returns the task    14 the task starts in a `match` arm
//!   5 a mapped array is mapped again    15 many workers, one-poll budget
//!   6 the same input mapped in a loop   16 `len` and indexing read the result
//!   7 two maps joined out of order      17 one mapper serves two maps
//!   8 an async helper returns the array 18 a mapper calls another function
//!   9 the result survives `gc_collect`  19 a mapper panic still aborts
//!  10 the input is a rooted temporary   20 the example is fully LIR

use std::time::Duration;

use super::support::{
    compile_and_run_with_env, compile_and_run_with_env_timeout, compile_with_compiler_env,
};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_BUDGET: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_TASK_BUDGET", "1"),
];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];

const IMPORTS: &str = "import std::collections::Array;\nimport std::parallel;\n";

/// Each named function must appear in the walker's selection log for `source`.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_LIR_LOG", "1"),
        ],
    );
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

/// `expected` must come out of all four configurations, and `functions` must
/// each be named in the walker's selection log.
fn assert_maps(source: &str, expected: &str, functions: &[&str]) {
    for env in [&AST[..], &LIR[..], &LIR_BUDGET[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_compiled(source, functions);
}

// 1. The runtime chunks the input across workers, but the result is assembled
//    in SOURCE order — otherwise nothing below could compare against a literal.
#[test]
fn lir_parallel_01_named_mapper_keeps_source_order() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn square(value: i64) -> i64 {{ return value * value; }}
async fn main() {{
    let values: Array<i64> = [5, 1, 4, 2, 3];
    println((await parallel::map(values.freeze(), square)).toString());
}}
"
        ),
        "[25, 1, 16, 4, 9]\n",
        &["square", "main"],
    );
}

// 2. A contextual lambda is the same kind of value as a named function: a bare
//    code address, because it captures nothing.
#[test]
fn lir_parallel_02_a_lambda_is_a_mapper_too() {
    assert_maps(
        &format!(
            "{IMPORTS}
async fn main() {{
    let values: Array<i64> = [1, 2, 3, 4];
    println((await parallel::map(values.freeze(), |value| value * 2)).toString());
}}
"
        ),
        "[2, 4, 6, 8]\n",
        &["$lambda.0", "main"],
    );
}

// 3. Neither degenerate input splits into chunks, so both take the runtime's
//    short paths — and the walker still has to hand back a real array.
#[test]
fn lir_parallel_03_empty_and_singleton_inputs_complete() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn increment(value: i64) -> i64 {{ return value + 1; }}
async fn main() {{
    let empty: Array<i64> = [];
    let one: Array<i64> = [41];
    println((await parallel::map(empty.freeze(), increment)).toString());
    println((await parallel::map(one.freeze(), increment)).toString());
}}
"
        ),
        "[]\n[42]\n",
        &["increment", "main"],
    );
}

// 4. The call produces an ordinary `Task<Array<i64>>` value, so a SYNC helper
//    may start the work and hand the handle back to be awaited elsewhere. The
//    helper is sync because it has no loop: a sync helper that loops is refused
//    in task context (E0810).
#[test]
fn lir_parallel_04_a_sync_helper_returns_the_task() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn plus_ten(value: i64) -> i64 {{ return value + 10; }}
fn start(values: FrozenArray<i64>) -> Task<Array<i64>> {{
    return parallel::map(values, plus_ten);
}}
async fn main() {{
    let values: Array<i64> = [1, 2, 3];
    println((await start(values.freeze())).toString());
}}
"
        ),
        "[11, 12, 13]\n",
        &["plus_ten", "start", "main"],
    );
}

// 5. The output of one map is the input of the next, once frozen — so the
//    walker roots a handle it received from the runtime, not only one it built.
#[test]
fn lir_parallel_05_a_mapped_array_is_mapped_again() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn twice(value: i64) -> i64 {{ return value * 2; }}
fn plus_one(value: i64) -> i64 {{ return value + 1; }}
async fn main() {{
    let values: Array<i64> = [1, 2, 3];
    let once = await parallel::map(values.freeze(), twice);
    let again = await parallel::map(once.freeze(), plus_one);
    println(again.toString());
}}
"
        ),
        "[3, 5, 7]\n",
        &["twice", "plus_one", "main"],
    );
}

// 6. The handle is rooted for the length of each call and popped when the call
//    returns, so the root depth is back where it started every time round.
#[test]
fn lir_parallel_06_the_same_input_mapped_in_a_loop() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn square(value: i64) -> i64 {{ return value * value; }}
async fn main() {{
    let values: Array<i64> = [5, 1, 4, 2, 3];
    let frozen = values.freeze();
    let mut total = 0;
    let mut i = 0;
    while i < 5 {{
        let mapped = await parallel::map(frozen, square);
        total = total + mapped[i];
        i = i + 1;
    }}
    println(total.toString());
}}
"
        ),
        "55\n",
        &["square", "main"],
    );
}

// 7. Two maps in flight at once, joined in the opposite order from the one they
//    were started in: the handles are independent values.
#[test]
fn lir_parallel_07_two_maps_joined_out_of_order() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn twice(value: i64) -> i64 {{ return value * 2; }}
fn plus_ten(value: i64) -> i64 {{ return value + 10; }}
async fn main() {{
    let left: Array<i64> = [1, 2, 3, 4];
    let right: Array<i64> = [9, 8, 7];
    let first = parallel::map(left.freeze(), twice);
    let second = parallel::map(right.freeze(), plus_ten);
    println((await second).toString());
    println((await first).toString());
}}
"
        ),
        "[19, 18, 17]\n[2, 4, 6, 8]\n",
        &["twice", "plus_ten", "main"],
    );
}

// 8. The mapped array crosses a function return. An `Array<T>` is not `Sync`
//    and so cannot cross a TASK boundary, but returning one from an awaited
//    helper is an ordinary move.
#[test]
fn lir_parallel_08_an_async_helper_returns_the_array() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn twice(value: i64) -> i64 {{ return value * 2; }}
async fn doubled(values: FrozenArray<i64>) -> Array<i64> {{
    return await parallel::map(values, twice);
}}
async fn main() {{
    let values: Array<i64> = [1, 2, 3, 4];
    println((await doubled(values.freeze())).toString());
}}
"
        ),
        "[2, 4, 6, 8]\n",
        &["twice", "doubled", "main"],
    );
}

// 9. The result is only reachable through the local that received it, and a
//    forced collection runs while a few hundred strings are being allocated
//    around it. Under `WILLOW_GC_STRESS=alloc` an unrooted slot is collected.
#[test]
fn lir_parallel_09_the_result_survives_gc_collect() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn square(value: i64) -> i64 {{ return value * value; }}
async fn main() {{
    let values: Array<i64> = [3, 4, 5];
    let mapped = await parallel::map(values.freeze(), square);
    let mut junk = \"\";
    let mut i = 0;
    while i < 200 {{ junk = junk + \"x\"; i = i + 1; }}
    gc_collect();
    println(mapped.toString());
}}
"
        ),
        "[9, 16, 25]\n",
        &["square", "main"],
    );
}

// 10. The frozen input is a TEMPORARY: `freeze()` allocates a view that nothing
//     else holds, and the runtime call that follows allocates the task. If the
//     walker did not root the argument, the collector would be free to take the
//     view between the arguments being evaluated and the runtime reading them —
//     which under `WILLOW_GC_STRESS=alloc` it would, on the very next
//     allocation.
#[test]
fn lir_parallel_10_the_input_is_a_rooted_temporary() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn square(value: i64) -> i64 {{ return value * value; }}
async fn main() {{
    let mut values: Array<i64> = [];
    let mut i = 0;
    while i < 64 {{ values.push(i); i = i + 1; }}
    let mapped = await parallel::map(values.freeze(), square);
    gc_collect();
    println(mapped.len().toString());
    println(mapped[63].toString());
}}
"
        ),
        "64\n3969\n",
        &["square", "main"],
    );
}

// 11. Cancelling reports `Cancelled` rather than a half-mapped array: there is
//     no partial success to observe. The input is large enough that the cancel
//     lands before the mapping finishes.
#[test]
fn lir_parallel_11_cancellation_has_no_partial_result() {
    let source = format!(
        "{IMPORTS}
fn identity(value: i64) -> i64 {{ return value; }}
async fn main() {{
    let mut values: Array<i64> = [];
    let mut i = 0;
    while i < 50000 {{ values.push(i); i = i + 1; }}
    let mapping = parallel::map(values.freeze(), identity);
    mapping.cancel();
    match await mapping.result() {{
        Ok(mapped) => println(\"unexpected success\"),
        Err(Cancelled) => println(\"cancelled\"),
    }}
}}
"
    );
    for env in [&AST[..], &LIR[..]] {
        let (out, ok, timed_out) =
            compile_and_run_with_env_timeout(&source, env, Duration::from_secs(30));
        assert!(!timed_out, "parallel cancellation parked forever: {out}");
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, "cancelled\n", "wrong output under {env:?}");
    }
    assert_walker_compiled(&source, &["identity", "main"]);
}

// 12. A `TaskScope` takes the handle like any other task's, so the mapping is
//     joined by the scope rather than by an `await` on the handle itself.
#[test]
fn lir_parallel_12_a_task_scope_owns_the_task() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn twice(value: i64) -> i64 {{ return value * 2; }}
async fn main() {{
    let values: Array<i64> = [4, 5];
    let scope = TaskScope::new();
    scope.add(parallel::map(values.freeze(), twice));
    match await scope.finish() {{
        Ok(done) => println(\"joined\"),
        Err(Cancelled) => println(\"cancelled\"),
    }}
}}
"
        ),
        "joined\n",
        &["twice", "main"],
    );
}

// 13. The handle survives in a class field, which is a traced slot rather than
//     a frame slot — so the task outlives the statement that started it.
#[test]
fn lir_parallel_13_the_handle_lives_in_a_class_field() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn twice(value: i64) -> i64 {{ return value * 2; }}
class Job {{ pub work: Task<Array<i64>>; }}
async fn main() {{
    let values: Array<i64> = [7, 8];
    let job = new Job(parallel::map(values.freeze(), twice));
    gc_collect();
    println((await job.work).toString());
}}
"
        ),
        "[14, 16]\n",
        &["twice", "main"],
    );
}

// 14. Starting the map inside a `match` arm and awaiting it after the match:
//     the arm's bracket restores the root depth, and the handle assigned
//     outward is still live at the `await`. (Awaiting INSIDE an arm is a
//     separate limit — an arm body has no cooperative await split of its own.)
#[test]
fn lir_parallel_14_the_task_starts_in_a_match_arm() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn twice(value: i64) -> i64 {{ return value * 2; }}
fn thrice(value: i64) -> i64 {{ return value * 3; }}
async fn main() {{
    let values: Array<i64> = [1, 2, 3];
    let frozen = values.freeze();
    let mut task = parallel::map(frozen, twice);
    match false {{
        true => {{ task = parallel::map(frozen, thrice); }},
        false => {{ task = parallel::map(frozen, twice); }},
    }}
    println((await task).toString());
}}
"
        ),
        "[2, 4, 6]\n",
        &["twice", "thrice", "main"],
    );
}

// 15. A worker per chunk and a one-poll budget: the mapping is re-polled across
//     many workers while the walker's frame holds the roots.
#[test]
fn lir_parallel_15_many_workers_and_a_one_poll_budget() {
    let source = format!(
        "{IMPORTS}
fn transform(value: i64) -> i64 {{ return value * 3 - 1; }}
async fn main() {{
    let mut values: Array<i64> = [];
    let mut i = 0;
    while i < 1000 {{ values.push(i); i = i + 1; }}
    let mapped = await parallel::map(values.freeze(), transform);
    gc_collect();
    println(mapped.len().toString());
    println(mapped[0].toString());
    println(mapped[511].toString());
    println(mapped[999].toString());
}}
"
    );
    let expected = "1000\n-1\n1532\n2996\n";
    for env in [
        &[("WILLOW_LIR_BACKEND", "0"), ("WILLOW_WORKERS", "16")][..],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_WORKERS", "16"),
            ("WILLOW_TASK_BUDGET", "1"),
        ][..],
    ] {
        let (out, ok, timed_out) =
            compile_and_run_with_env_timeout(&source, env, Duration::from_secs(60));
        assert!(!timed_out, "mapping parked forever under {env:?}: {out}");
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_compiled(&source, &["transform", "main"]);
}

// 16. `len` and indexing are the ordinary `Array<i64>` methods: what comes back
//     is a real array, not an opaque handle that only knows `toString`.
#[test]
fn lir_parallel_16_len_and_indexing_read_the_result() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn square(value: i64) -> i64 {{ return value * value; }}
async fn main() {{
    let values: Array<i64> = [6, 7, 8];
    let mapped = await parallel::map(values.freeze(), square);
    println(mapped.len().toString());
    println(mapped[0].toString());
    println(mapped[2].toString());
}}
"
        ),
        "3\n36\n64\n",
        &["square", "main"],
    );
}

// 17. One mapper, two calls with different inputs. Nothing about the function
//     value is per-call, so the same address is passed twice.
#[test]
fn lir_parallel_17_one_mapper_serves_two_maps() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn square(value: i64) -> i64 {{ return value * value; }}
async fn main() {{
    let left: Array<i64> = [1, 2];
    let right: Array<i64> = [3, 4];
    println((await parallel::map(left.freeze(), square)).toString());
    println((await parallel::map(right.freeze(), square)).toString());
}}
"
        ),
        "[1, 4]\n[9, 16]\n",
        &["square", "main"],
    );
}

// 18. The mapper is not a leaf: it calls another function, so the chunk task
//     runs a real call stack rather than a single arithmetic expression.
#[test]
fn lir_parallel_18_a_mapper_calls_another_function() {
    assert_maps(
        &format!(
            "{IMPORTS}
fn triple(value: i64) -> i64 {{ return value * 3; }}
fn shifted(value: i64) -> i64 {{ return triple(value) + 1; }}
async fn main() {{
    let values: Array<i64> = [1, 2, 3];
    println((await parallel::map(values.freeze(), shifted)).toString());
}}
"
        ),
        "[4, 7, 10]\n",
        &["triple", "shifted", "main"],
    );
}

// 19. A panic inside a mapper is a task abort on both backends: the process
//     dies with the panic reported, and no partial array is printed.
#[test]
fn lir_parallel_19_a_mapper_panic_still_aborts() {
    let source = format!(
        "{IMPORTS}
fn checked(value: i64) -> i64 {{
    if value == 2 {{ panic(\"parallel mapper failed\"); }}
    return value;
}}
async fn main() {{
    let values: Array<i64> = [1, 2, 3];
    println((await parallel::map(values.freeze(), checked)).toString());
}}
"
    );
    for env in [&AST[..], &LIR[..]] {
        let (out, ok) = compile_and_run_with_env(&source, env);
        assert!(!ok, "a mapper panic must abort under {env:?}: {out}");
        assert!(
            out.contains("runtime panic: parallel mapper failed"),
            "no panic report under {env:?}: {out}"
        );
    }
    assert_walker_compiled(&source, &["checked", "main"]);
}

// 20. The checked-in example compiles with every function on the walker, which
//     is what removes `parallel::map` from the AST-fallback census.
#[test]
fn lir_parallel_20_the_example_is_fully_lir() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/example/lir_parallel_map.wi"
    ))
    .expect("the example is checked in");
    assert_walker_compiled(
        &source,
        &[
            "square",
            "plus_ten",
            "squares",
            "start",
            "counting",
            "twice_over",
            "repeated",
            "interleaved",
            "cancelled",
            "$lambda.0",
            "main",
        ],
    );
}
