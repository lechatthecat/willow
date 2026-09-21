use crate::support::*;

// ── Bounded parallel mapping (willow-2s3.4) ────────────────────────────────
// The public v1 surface is deliberately narrow: immutable i64 input and a
// non-capturing scalar function. Runtime unit tests pin chunk-count bounds and
// exact range coverage; these tests pin the language contract, ordering,
// cancellation, nesting, panic policy, aliases, and GC/high-worker behavior.

#[test]
fn parallel_map_01_named_mapper_preserves_input_order() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
import std::parallel;
fn square(value: i64) -> i64 { return value * value; }
async fn main() {
    let values: Array<i64> = [5, 1, 4, 2, 3];
    println((await parallel::map(values.freeze(), square)).toString());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[25, 1, 16, 4, 9]\n");
}

#[test]
fn parallel_map_02_contextual_lambda_and_module_alias_work() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
import std::parallel as par;
async fn main() {
    let values: Array<i64> = [1, 2, 3];
    println((await par::map(values.freeze(), |value| value * 2)).toString());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[2, 4, 6]\n");
}

#[test]
fn parallel_map_03_empty_and_singleton_inputs_complete() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
import std::parallel;
fn increment(value: i64) -> i64 { return value + 1; }
async fn main() {
    let empty: Array<i64> = [];
    let one: Array<i64> = [41];
    println((await parallel::map(empty.freeze(), increment)).toString());
    println((await parallel::map(one.freeze(), increment)).toString());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[]\n[42]\n");
}

#[test]
fn parallel_map_04_two_nested_scheduler_maps_can_run_concurrently() {
    let (out, ok) = compile_and_run_with_env(
        r#"
import std::collections::Array;
import std::parallel;
fn twice(value: i64) -> i64 { return value * 2; }
fn plus_ten(value: i64) -> i64 { return value + 10; }
async fn nested_twice(values: FrozenArray<i64>) -> Array<i64> {
    return await parallel::map(values, twice);
}
async fn nested_plus_ten(values: FrozenArray<i64>) -> Array<i64> {
    return await parallel::map(values, plus_ten);
}
async fn main() {
    let left: Array<i64> = [1, 2, 3, 4];
    let right: Array<i64> = [9, 8, 7];
    let first = nested_twice(left.freeze());
    let second = nested_plus_ten(right.freeze());
    println((await second).toString());
    println((await first).toString());
}
"#,
        &[("WILLOW_WORKERS", "5"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[19, 18, 17]\n[2, 4, 6, 8]\n");
}

#[test]
fn parallel_map_05_gc_stress_and_high_worker_count_keep_results_rooted() {
    let (out, ok) = compile_and_run_with_env(
        r#"
import std::collections::Array;
import std::parallel;
fn transform(value: i64) -> i64 { return value * 3 - 1; }
async fn main() {
    let mut values: Array<i64> = [];
    let mut i = 0;
    while i < 1000 { values.push(i); i = i + 1; }
    let result = await parallel::map(values.freeze(), transform);
    gc_collect();
    println(result.len());
    println(result[0]);
    println(result[511]);
    println(result[999]);
}
"#,
        &[
            ("WILLOW_WORKERS", "32"),
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1000\n-1\n1532\n2996\n");
}

#[test]
fn parallel_map_06_cancellation_has_no_partial_success_result() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
import std::collections::Array;
import std::parallel;
fn identity(value: i64) -> i64 { return value; }
async fn main() {
    let mut values: Array<i64> = [];
    let mut i = 0;
    while i < 50000 { values.push(i); i = i + 1; }
    let mapping = parallel::map(values.freeze(), identity);
    mapping.cancel();
    match await mapping.result() {
        Ok(result) => println("unexpected success"),
        Err(Cancelled) => println("cancelled"),
    }
}
"#,
        &[("WILLOW_WORKERS", "5"), ("WILLOW_TASK_BUDGET", "1")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "parallel cancellation parked forever: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\n");
}

#[test]
fn parallel_map_07_mutable_array_input_is_rejected() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;
import std::parallel;
fn identity(value: i64) -> i64 { return value; }
async fn main() {
    let values: Array<i64> = [1, 2];
    let result = await parallel::map(values, identity);
}
"#,
        &["error[E0201]", "FrozenArray<i64>"],
    );
}

#[test]
fn parallel_map_08_wrong_mapper_signature_is_rejected() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;
import std::parallel;
fn stringify(value: i64) -> String { return value.toString(); }
async fn main() {
    let values: Array<i64> = [1, 2];
    let result = await parallel::map(values.freeze(), stringify);
}
"#,
        &["error[E0201]", "fn(i64) -> i64"],
    );
}

/// `parallel::map`'s v1 mapper is a `fn(i64) -> i64` — a bare code address the
/// worker calls with no environment — so a capturing lambda is refused there
/// even though captures are legal in general (willow-0g8j.2.12). The refusal
/// has to name the capture, since the fix is to stop capturing rather than to
/// change the mapper.
#[test]
fn parallel_map_09_captured_mapper_is_rejected() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;
import std::parallel;
async fn main() {
    let offset = 10;
    let values: Array<i64> = [1, 2];
    let result = await parallel::map(values.freeze(), |value| value + offset);
}
"#,
        &[
            "error[E1011]",
            "capturing lambda cannot be used as a plain function pointer",
            "captures `offset`",
        ],
    );
}

#[test]
fn parallel_map_10_mapper_panic_uses_task_abort_policy() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;
import std::parallel;
fn checked(value: i64) -> i64 {
    if value == 2 { panic("parallel mapper failed"); }
    return value;
}
async fn main() {
    let values: Array<i64> = [1, 2, 3];
    println((await parallel::map(values.freeze(), checked)).toString());
}
"#,
    );
    assert!(!ok, "mapper panic must abort the process: {out}");
    assert!(
        out.contains("runtime panic: parallel mapper failed"),
        "{out}"
    );
}
