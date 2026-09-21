use super::*;

// ── Trap contract sweep (willow-l9lx bug CLASS detector) ────────────────────
// Every aborting runtime failure in a DEBUG build must present as a located
// `runtime panic:` message — never a silent raw hardware signal (which prints
// nothing). One table; new trappable constructs must join it. A row failing
// with EMPTY output means a raw SIGILL/SIGFPE regression of the l9lx class.
#[test]
fn trap_contract_all_aborts_have_panic_messages() {
    let scenarios: &[(&str, &str)] = &[
        (
            // The sleep must outlast ANY stall the spawner can suffer between
            // the spawn and the `cancel()`. The task is published to the shared
            // run queues and polled by a peer worker immediately, so with a
            // short sleep it can reach `return 1` before `h.cancel()` lands and
            // the await then succeeds instead of aborting — a load-sensitive
            // flake that only showed up when the whole suite ran in parallel
            // (willow-fqzz). An hour is not a wait: `cancel()` re-queues the
            // task and the await aborts in milliseconds. If cancellation ever
            // stops landing, the scenario parks instead of finishing, which is
            // why every row below runs under a hard deadline.
            "await of a cancelled task",
            "async fn t() -> i64 { await sleep(3600000); return 1; } async fn main() { let h = t(); h.cancel(); println(await h); }",
        ),
        (
            "int division by zero",
            "fn f(a: i64, b: i64) -> i64 { return a / b; } fn main() { println(f(1, 0)); }",
        ),
        (
            "int remainder by zero",
            "fn f(a: i64, b: i64) -> i64 { return a % b; } fn main() { println(f(1, 0)); }",
        ),
        (
            "i64::MIN / -1 overflow",
            "fn f(a: i64, b: i64) -> i64 { return a / b; } fn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
        ),
        (
            "i64::MIN % -1 overflow",
            "fn f(a: i64, b: i64) -> i64 { return a % b; } fn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
        ),
        (
            "array index out of bounds",
            "import std::collections::Array; fn main() { let xs: Array<i64> = [1]; println(xs[5]); }",
        ),
        (
            "array negative index",
            "import std::collections::Array; fn main() { let xs: Array<i64> = [1]; println(xs[0 - 1]); }",
        ),
        (
            "pop from empty array",
            "import std::collections::Array; fn main() { let mut xs: Array<i64> = [1]; xs.pop(); xs.pop(); }",
        ),
        (
            "array element write out of bounds",
            "import std::collections::Array; fn main() { let xs: Array<i64> = [1]; xs[5] = 9; }",
        ),
        // (invalid reference field access is CHECKER-prevented in every reachable
        // form — direct, aliased, nested, narrowing-then-mutate — so it has no
        // runtime row; the backend guard remains defense-in-depth.)
        ("explicit panic()", "fn main() { panic(\"boom\"); }"),
    ];
    for (what, source) in scenarios {
        // Guard against a silently-uncompilable row: a compile failure would
        // otherwise masquerade as the expected abort.
        let (compiles, stderr) = compile_with_compiler_env(source, &[]);
        assert!(compiles, "{what}: scenario must compile, got: {stderr}");
        // Under a deadline: an aborting row takes milliseconds, so a row that
        // parks instead has failed the contract just as surely as one that
        // exits 0, and it must say so rather than hang the suite.
        let (out, ok, timed_out) =
            compile_and_run_with_env_timeout(source, &[], std::time::Duration::from_secs(60));
        assert!(
            !timed_out,
            "{what}: never aborted — the scenario parked instead. output: {out:?}"
        );
        assert!(!ok, "{what}: expected an abort, got success: {out}");
        assert!(
            out.contains("runtime panic:") || out.contains("panic:"),
            "{what}: aborted with NO panic message (raw signal — l9lx-class \
             regression). output: {out:?}"
        );
    }
}
