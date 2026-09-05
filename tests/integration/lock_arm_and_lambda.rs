//! End-to-end guard for match-arm lock lowering and lambda boundaries.
//!
//! Two leaks put a `lock` (and an `await`) somewhere no backend has a resume
//! point, and each ended in an abort rather than a diagnostic:
//!
//! * a `match` arm body now becomes explicit LIR CFG, so its lock acquisition
//!   owns a frame-backed resume edge (willow-0g8j.3);
//! * a lambda body -- the type checker inherited the enclosing `async fn`'s
//!   context across a boundary the backend treats as a separate lifted
//!   function, so E2603 and E0801 both waved the body through (willow-3kty).
//!
//! The unit tests in `src/semantic/type_checker/mod.rs` cover which diagnostic
//! each shape gets. These run the real `willowc` binary, because the property
//! that regressed is one only the process can show: it exits with an error
//! instead of aborting. Every case asserts the absence of a panic explicitly,
//! since a panicking compiler also "fails to compile" and would otherwise pass
//! a plain error-expected assertion.
//!
//!   1 lock in an arm             5 lock in a section's lambda
//!   2 lock in an arm's lambda    6 await in a lambda
//!   3 lock in a lambda           7 the accepted rewrites still run
//!   4 lock in an arm, RwLock     8 the example runs

use super::support::{compile_and_run_with_env, compile_error_stderr};

/// A compiler abort leaves one of these in stderr. None may appear: the point
/// of every case below is that the compiler REPORTS rather than dies.
const ABORT_MARKERS: &[&str] = &[
    "panicked at",
    "internal compiler error",
    "internal error: entered unreachable code",
    "RUST_BACKTRACE",
];

/// Compile `source`, require it to fail with `expected_code`, and require the
/// failure to be a diagnostic rather than a crash.
fn assert_rejected_without_aborting(source: &str, expected_code: &str) {
    let stderr = compile_error_stderr(source);
    for marker in ABORT_MARKERS {
        assert!(
            !stderr.contains(marker),
            "the compiler aborted instead of reporting {expected_code}; \
             stderr contained `{marker}`:\n{stderr}"
        );
    }
    assert!(
        stderr.contains(expected_code),
        "expected {expected_code}:\n{stderr}"
    );
}

/// A sync taker, so a lambda can be written as an argument. An `fn`-typed
/// local inside an async frame is separately rejected by E2402, which would
/// mask the diagnostic under test.
const APPLY: &str = "fn apply(m: Mutex<i64>, f: fn(Mutex<i64>) -> void) { f(m); }\n";

// 1. The original report now compiles and resumes through LIR.
#[test]
fn lock_arm_e2e_01_lock_in_a_match_arm_resumes() {
    let source = "async fn pick(m: Mutex<i64>, which: i64) -> i64 {
             match which {
                 1 => { lock m as mut v { v = v + 1; } return 1; }
                 _ => { return 0; }
             }
         }
         async fn main() { let m = Mutex::new(0); println(await pick(m, 1)); }";
    let (out, ok) = compile_and_run_with_env(source, &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

// 2. The same statement one level further in, behind a lambda. Both leaks
// apply, and the lambda boundary wins: the body is its own function, so the
// fact reported is that it is not an async one.
#[test]
fn lock_arm_e2e_02_lock_in_an_arms_lambda_is_reported_not_aborted() {
    assert_rejected_without_aborting(
        &format!(
            "{APPLY}
             async fn outer(m: Mutex<i64>, x: i64) {{
                 match x {{
                     1 => {{ apply(m, |c| {{ lock c as mut v {{ v = v + 1; }} }}); }}
                     _ => {{}}
                 }}
             }}
             async fn main() {{ let m = Mutex::new(0); await outer(m, 1); }}"
        ),
        "E2603",
    );
}

// 3. No `match` at all. The lambda leak never needed one, so this is the
// shortest program that used to abort with `variable `v` reached LIR codegen
// unbound`.
#[test]
fn lock_arm_e2e_03_lock_in_a_lambda_is_reported_not_aborted() {
    assert_rejected_without_aborting(
        &format!(
            "{APPLY}
             async fn outer(m: Mutex<i64>) {{
                 apply(m, |c| {{ lock c as mut v {{ v = v + 1; }} }});
             }}
             async fn main() {{ let m = Mutex::new(0); await outer(m); }}"
        ),
        "E2603",
    );
}

// 4. `lock write` takes the same resumable arm path as the Mutex form.
#[test]
fn lock_arm_e2e_04_rwlock_write_in_a_match_arm_resumes() {
    let source = "async fn pick(l: RwLock<i64>, which: i64) -> i64 {
             match which {
                 1 => { lock write l as mut v { v = v + 1; } return 1; }
                 _ => { return 0; }
             }
         }
         async fn main() { let l = RwLock::new(0); println(await pick(l, 1)); }";
    let (out, ok) = compile_and_run_with_env(source, &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

// 5. A lambda CONSTRUCTED inside a critical section. Before the boundary reset
// this reported E2605 -- a nested acquisition -- which is the wrong fact: the
// body holds nothing when it runs. It must report the boundary fact instead,
// and must not report nesting.
#[test]
fn lock_arm_e2e_05_lock_in_a_sections_lambda_is_not_called_nesting() {
    let source = format!(
        "{APPLY}
         async fn outer(m: Mutex<i64>, n: Mutex<i64>) {{
             lock m as mut a {{
                 a = a + 1;
                 apply(n, |c| {{ lock c as mut v {{ v = v + 1; }} }});
             }}
         }}
         async fn main() {{
             let m = Mutex::new(0);
             let n = Mutex::new(0);
             await outer(m, n);
         }}"
    );
    assert_rejected_without_aborting(&source, "E2603");
    let stderr = compile_error_stderr(&source);
    assert!(
        !stderr.contains("E2605"),
        "a lambda body holds no outer lock, so this is not nesting:\n{stderr}"
    );
}

// 6. `await` rode the same lambda leak into a different abort -- `a LIR
// value-position await reached emission unsplit` -- so it is the second half
// of the same regression.
#[test]
fn lock_arm_e2e_06_await_in_a_lambda_is_reported_not_aborted() {
    assert_rejected_without_aborting(
        "async fn helper() -> i64 { await sleep(1); return 7; }
         fn call(f: fn(i64) -> i64) -> i64 { return f(1); }
         async fn outer() -> i64 { return call(|n| n + await helper()); }
         async fn main() { println(await outer()); }",
        "E0801",
    );
}

// 7. Rejecting is only half a fix. The three rewrites the E2606 help names
// have to actually compile and run, or the diagnostic points nowhere.
#[test]
fn lock_arm_e2e_07_the_named_rewrites_compile_and_run() {
    let source = "
        async fn add_to(m: Mutex<i64>, amount: i64) -> i64 {
            lock m as mut v { v = v + amount; }
            return amount;
        }
        // Rewrite 1: the `match` moves inside the critical section.
        async fn inside(m: Mutex<i64>, which: i64) -> i64 {
            lock m as mut v {
                match which { 1 => { v = v + 1; } _ => { v = v + 2; } }
                return v;
            }
        }
        // Rewrite 2: `if`/`else`, whose statements stay in the enclosing
        // sequence and keep their resume point.
        async fn branch(m: Mutex<i64>, which: i64) -> i64 {
            if which == 1 {
                lock m as mut v { v = v + 10; }
                return 1;
            } else {
                lock m as mut v { v = v + 20; }
                return 2;
            }
        }
        // Rewrite 3: an `async fn` that takes the lock at its own top level,
        // awaited from the arm.
        async fn dispatch(m: Mutex<i64>, which: i64) -> i64 {
            match which {
                1 => { return await add_to(m, 100); }
                _ => { return await add_to(m, 3); }
            }
        }
        async fn main() {
            let m = Mutex::new(0);
            println(await inside(m, 1));
            println(await branch(m, 1));
            println(await dispatch(m, 1));
            println(await dispatch(m, 9));
            lock m as v { println(v); }
        }";
    let (out, ok) = compile_and_run_with_env(source, &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n1\n100\n3\n114\n");
}

// 8. The runnable example, plainly and under GC stress. It is the one place all
// three rewrites run under real contention.
#[test]
fn lock_arm_e2e_08_example_is_stable_under_gc_stress() {
    let source = include_str!("../../example/lock_match_arm.wi");
    let (out, ok) = compile_and_run_with_env(source, &[]);
    assert!(ok, "the example must run: {out}");
    let (stressed, stressed_ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(stressed_ok, "{stressed}");
    assert_eq!(stressed, out, "GC stress must not change the output");
}
