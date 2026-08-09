use super::support::*;

// Review follow-ups for the panic/recover epic (willow-s9ej.7 review):
//
//   1. a normal-exit defer — in any frame — must not be able to consume a
//      panic that is unwinding some OTHER scope,
//   2. the scheduler re-entry guard must stay closed for the whole cleanup
//      invocation, including after `recover()` consumed the panic,
//   3. an unhandled async panic publishes the terminal state and aborts without
//      letting an awaiter run its continuation,
//   4. `recover` is a reserved builtin name at every binding site,
//   5. `PanicInfo` fields are read-only through `&mut` as well as assignment,
//   6. runtime-raised faults record a source location, not `:0:0`, and that
//      location is the code that faulted — including inside a callee compiled
//      through the LIR backend.
//
// Perspectives 01-28 below; each test name states the perspective it pins.

// --- 1. capability boundary --------------------------------------------------

/// Perspective 01. A helper invoked FROM panic cleanup runs its own defers on
/// normal exit. Those defers are not the cleanup of the unwinding scope, so
/// they must not steal the caller's panic: the panic stays unhandled.
#[test]
fn prr_01_normal_exit_defer_in_a_helper_cannot_steal_the_callers_panic() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn thief() {
    defer match recover() {
        Some(info) => println("stolen:" + info.message),
        None => println("none")
    }
    println("thief");
}

fn main() {
    if true {
        defer thief();
        panic("boom");
    }
    println("unreachable");
}
"#,
    );
    assert!(!ok, "the stolen panic must stay unhandled: {out}");
    assert!(out.contains("none"), "{out}");
    assert!(!out.contains("stolen"), "{out}");
    assert!(!out.contains("unreachable"), "{out}");
    assert!(out.contains("boom"), "{out}");
}

/// Perspective 02. The defer of the scope that is actually unwinding still
/// recovers — the fix narrows the capability, it does not remove it.
#[test]
fn prr_02_direct_cleanup_defer_still_recovers() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("recovered:" + info.message),
            None => println("none")
        }
        panic("boom");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "recovered:boom\nafter\n");
}

/// Perspective 03. A lambda called from panic cleanup is an ordinary call:
/// its own defer runs on normal exit and sees no recoverable panic.
#[test]
fn prr_03_lambda_called_from_cleanup_has_no_capability() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                let probe = || {
                    defer match recover() {
                        Some(_) => println("lambda-stole"),
                        None => println("lambda-none")
                    }
                    return 0;
                };
                let _ = probe();
                println("outer:" + info.message);
            },
            None => {}
        }
        panic("boom");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "lambda-none\nouter:boom\nafter\n");
}

/// Perspective 04. A helper called from panic cleanup can still recover a panic
/// that IT raised: capability follows the cleanup that is actually running.
#[test]
fn prr_04_helper_recovers_its_own_panic_during_outer_cleanup() {
    let (out, ok) = compile_and_run(
        r#"
fn helper() {
    if true {
        defer match recover() {
            Some(info) => println("inner:" + info.message),
            None => println("inner-none")
        }
        panic("inner-boom");
    }
}

fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                helper();
                println("outer:" + info.message);
            },
            None => {}
        }
        panic("outer-boom");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "inner:inner-boom\nouter:outer-boom\nafter\n");
}

/// Perspective 05. Recovery consumes the panic exactly once, so a helper called
/// after `recover()` inside the same defer body sees nothing to recover.
#[test]
fn prr_05_helper_after_recovery_sees_no_panic() {
    let (out, ok) = compile_and_run(
        r#"
fn probe() {
    defer match recover() {
        Some(_) => println("probe-recovered"),
        None => println("probe-none")
    }
    println("probe");
}

fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                println("outer:" + info.message);
                probe();
            },
            None => {}
        }
        panic("boom");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "outer:boom\nprobe\nprobe-none\nafter\n");
}

/// Perspective 06. A plain `return recover();` helper never inherits the
/// caller's capability, whether or not the caller is unwinding.
#[test]
fn prr_06_direct_recover_helper_returns_none_outside_cleanup() {
    let (out, ok) = compile_and_run(
        r#"
fn hidden() -> Option<PanicInfo> { return recover(); }

fn main() {
    match hidden() {
        Some(_) => println("recovered"),
        None => println("none")
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "none\n");
}

// --- 2. scheduler re-entry guard ---------------------------------------------

/// Perspective 07. The scheduler cannot be driven from a defer body at all: the
/// checker rejects `await`/`select` there, so the language-level half of the
/// re-entry guard is a compile error rather than a runtime abort. The runtime
/// half (a cleanup invocation that reaches the scheduler through the async
/// cancel path) is pinned by `panic_context_15` in the runtime crate.
#[test]
fn prr_07_scheduler_reentry_from_cleanup_is_rejected_at_compile_time() {
    let source = r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(42);
    return 0;
}

fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    if true {
        defer match recover() {
            Some(info) => {
                println("recovered:" + info.message);
                select {
                    let v = ch.recv() => { println(v); }
                }
            },
            None => {}
        }
        panic("boom");
    }
    println("after");
}
"#;
    assert_compile_error_contains(source, &["E0905", "not allowed inside a `defer`"]);
}

/// Perspective 08. The guard is scoped to the cleanup invocation, not to "a
/// panic happened here once": once the deferred block that recovered has
/// returned, `panic_defer_depth` is back to 0 and ordinary code may drive the
/// scheduler again.
#[test]
fn prr_08_scheduler_runs_again_after_a_recovered_panic() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(42);
    return 0;
}

fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    if true {
        defer match recover() {
            Some(info) => { println("recovered:" + info.message); },
            None => {}
        }
        panic("boom");
    }
    select {
        let v = ch.recv() => { println(v); }
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "recovered:boom\n42\nafter\n");
}

// --- 3. unhandled async panic ordering ---------------------------------------

/// Perspective 09. An unhandled task panic aborts the process, and no awaiter of
/// that task may run its continuation in the window between publishing the
/// terminal state (which wakes awaiters) and the abort.
#[test]
fn prr_09_awaiter_never_resumes_past_a_fatally_panicked_task() {
    let source = r#"
async fn faulty() {
    await sleep(1);
    panic("task-fault");
}

async fn watcher() {
    let bad = faulty();
    await bad;
    println("watcher-resumed");
}

async fn main() {
    let w = watcher();
    await w;
    println("main-resumed");
}
"#;
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        source,
        &[("WILLOW_WORKERS", "4")],
        std::time::Duration::from_secs(60),
    );
    assert!(!timed_out, "unhandled task panic hung: {out}");
    assert!(!ok, "an unhandled task panic must fail the process: {out}");
    assert!(out.contains("task-fault"), "{out}");
    assert!(!out.contains("watcher-resumed"), "{out}");
    assert!(!out.contains("main-resumed"), "{out}");
}

/// Perspective 10. The fatal path still publishes the full diagnostic before it
/// aborts: message, source location, and the async task chain.
#[test]
fn prr_10_unhandled_task_panic_reports_before_aborting() {
    let source = r#"
async fn faulty() {
    await sleep(1);
    panic("reported");
}

async fn main() {
    let bad = faulty();
    await bad;
    println("unreachable");
}
"#;
    let (out, ok, timed_out) =
        compile_and_run_with_env_timeout(source, &[], std::time::Duration::from_secs(60));
    assert!(!timed_out, "unhandled task panic hung: {out}");
    assert!(!ok, "{out}");
    assert!(out.contains("runtime panic:"), "{out}");
    assert!(out.contains("reported"), "{out}");
    assert!(out.contains("async stack (current task first)"), "{out}");
    assert!(!out.contains("unreachable"), "{out}");
}

// --- 4. `recover` is a reserved binding name ---------------------------------

/// Perspective 11. A local binding named `recover` would be silently ignored at
/// the call site (the builtin wins), so it is rejected instead.
#[test]
fn prr_11_let_binding_named_recover_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let recover = || { return 7; };
    println(recover());
}
"#,
        &["E0351", "reserved builtin function"],
    );
}

/// Perspective 12. Same for a parameter.
#[test]
fn prr_12_parameter_named_recover_is_rejected() {
    assert_compile_error_contains(
        r#"
fn helper(recover: i64) -> i64 { return recover; }
fn main() { println(helper(1)); }
"#,
        &["E0351", "reserved builtin function"],
    );
}

/// Perspective 13. Same for a `for` loop binding.
#[test]
fn prr_13_for_binding_named_recover_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    for recover in 0..2 {
        println(recover);
    }
}
"#,
        &["E0351", "reserved builtin function"],
    );
}

/// Perspective 14. Same for a lambda parameter.
#[test]
fn prr_14_lambda_parameter_named_recover_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let f = |recover: i64| { return recover; };
    println(f(1));
}
"#,
        &["E0351", "reserved builtin function"],
    );
}

/// Perspective 15. Same for a match pattern binding.
#[test]
fn prr_15_match_binding_named_recover_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value: Option<i64> = Some(1);
    match value {
        Some(recover) => println(recover),
        None => println(0)
    }
}
"#,
        &["E0351", "reserved builtin function"],
    );
}

/// Perspective 16. A top-level `fn recover` stays rejected (unchanged rule).
#[test]
fn prr_16_top_level_fn_recover_is_still_rejected() {
    assert_compile_error_contains(
        r#"
fn recover() -> i64 { return 1; }
fn main() { println(recover()); }
"#,
        &["E0351", "cannot redeclare `recover`"],
    );
}

/// Perspective 17. The reservation does not over-reach: a class member named
/// `recover` is reached through a receiver, never through bare-name dispatch.
#[test]
fn prr_17_class_member_named_recover_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    recover: i64;

    pub init(self, value: i64) {
        self.recover = value;
    }

    pub fn recover(self) -> i64 {
        return self.recover;
    }
}

fn main() {
    let b = new Box(5);
    println(b.recover());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

// --- 5. `PanicInfo` is read-only ---------------------------------------------

/// Perspective 18. `&mut info.field` would write through the runtime's panic
/// metadata, so it is rejected like a direct assignment.
#[test]
fn prr_18_mutable_reference_to_panic_info_field_is_rejected() {
    assert_compile_error_contains(
        r#"
fn overwrite(value: &mut i64) { value = 99; }

fn alter(info: PanicInfo) { overwrite(&info.line); }

fn main() {
    if true {
        defer match recover() {
            Some(info) => alter(info),
            None => {}
        }
        panic("boom");
    }
}
"#,
        &["fields of `PanicInfo` are read-only"],
    );
}

/// Perspective 19. A read-only `&` borrow of a `PanicInfo` field stays legal —
/// the restriction is on mutation, not on reading.
#[test]
fn prr_19_shared_reference_to_panic_info_field_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
fn peek(value: &i64) { println(value); }

fn show(info: PanicInfo) { peek(&info.line); }

fn main() {
    if true {
        defer match recover() {
            Some(info) => show(info),
            None => {}
        }
        panic("boom");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert!(out.ends_with("after\n"), "{out}");
    assert!(
        !out.starts_with("0\n"),
        "expected a real line number: {out}"
    );
}

/// Perspective 20. Direct field assignment stays rejected (unchanged rule).
#[test]
fn prr_20_panic_info_field_assignment_is_still_rejected() {
    assert_compile_error_contains(
        r#"
fn alter(info: PanicInfo) { info.message = "changed"; }
fn main() { println("x"); }
"#,
        &["fields of `PanicInfo` are read-only"],
    );
}

/// Perspective 21. The restriction does not over-reach: `&mut` on an ordinary
/// class field still works.
#[test]
fn prr_21_mutable_reference_to_an_ordinary_field_still_works() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub value: i64;

    pub init(self, value: i64) {
        self.value = value;
    }
}

fn bump(value: &mut i64) { value = value + 1; }

fn main() {
    let c = new Counter(1);
    bump(&c.value);
    println(c.value);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// --- 6. runtime faults carry a source location -------------------------------

/// Perspective 22. An out-of-bounds index is raised inside the runtime, which
/// has no span of its own; debug builds publish the statement's location so the
/// recovered `PanicInfo` is not `:0:0`.
#[test]
fn prr_22_array_bounds_fault_records_a_source_location() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let items = [1, 2, 3];
    if true {
        defer match recover() {
            Some(info) => {
                println("empty_file=" + (info.file == "").toString());
                println("line=" + info.line.toString());
                println("positive_column=" + (info.column > 0).toString());
            },
            None => println("none")
        }
        println(items[7]);
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "empty_file=false\nline=13\npositive_column=true\nafter\n"
    );
}

/// Perspective 23. `Option::unwrap` on `None` reports the call's own span.
#[test]
fn prr_23_option_unwrap_fault_records_the_call_location() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let value: Option<i64> = None;
    if true {
        defer match recover() {
            Some(info) => println("line=" + info.line.toString()),
            None => println("none")
        }
        println(value.unwrap());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "line=9\nafter\n");
}

/// Perspective 24. `Result::expect` reports its own span too, with the caller's
/// message preserved.
#[test]
fn prr_24_result_expect_fault_records_message_and_location() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let value: Result<i64, String> = Err("io");
    if true {
        defer match recover() {
            Some(info) => {
                println(info.message);
                println("line=" + info.line.toString());
            },
            None => println("none")
        }
        println(value.expect("needed a value"));
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "needed a value\nline=12\nafter\n");
}

/// Perspective 25. Awaiting a cancelled task raises from the scheduler, which
/// also has no span of its own; the await statement supplies one.
#[test]
fn prr_25_cancelled_await_fault_records_a_source_location() {
    let source = r#"
async fn slow() -> i64 {
    await sleep(50);
    return 1;
}

async fn main() {
    let t = slow();
    t.cancel();
    if true {
        defer match recover() {
            Some(info) => {
                println(info.message);
                println("empty_file=" + (info.file == "").toString());
                println("positive_line=" + (info.line > 0).toString());
            },
            None => println("none")
        }
        println(await t);
    }
    println("after");
}
"#;
    let (out, ok, timed_out) =
        compile_and_run_with_env_timeout(source, &[], std::time::Duration::from_secs(60));
    assert!(!timed_out, "cancelled await hung: {out}");
    assert!(ok, "{out}");
    assert!(out.contains("cancelled"), "{out}");
    assert!(out.contains("empty_file=false"), "{out}");
    assert!(out.contains("positive_line=true"), "{out}");
    assert!(out.contains("after"), "{out}");
}

/// Perspective 26. Release builds keep the source path in `PanicInfo`: the
/// V1 policy records message AND source location in both build modes, even
/// though the debug-only statement marker is not emitted.
#[test]
fn prr_26_release_build_keeps_the_panic_source_path() {
    let source = r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                println("empty_file=" + (info.file == "").toString());
                println("line=" + info.line.toString());
            },
            None => println("none")
        }
        panic("boom");
    }
    println("after");
}
"#;
    let (debug_out, debug_ok) = compile_and_run(source);
    assert!(debug_ok, "{debug_out}");
    assert_eq!(debug_out, "empty_file=false\nline=11\nafter\n");

    let (release_out, release_ok) = compile_and_run_release(source);
    assert!(release_ok, "{release_out}");
    assert_eq!(release_out, debug_out, "release build diverged");
}

/// Perspective 27. A fault raised inside a callee reports the CALLEE's line.
/// Simple functions compile through the LIR backend, which has its own emitter:
/// without a fault site of its own it would inherit the caller's statement and
/// point the report at the wrong function.
#[test]
fn prr_27_fault_inside_a_callee_reports_the_callees_line() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn lookup(values: Array<i64>, index: i64) -> i64 {
    let value = values[index];
    return value;
}

fn main() {
    let values: Array<i64> = [1, 2, 3];
    if true {
        defer match recover() {
            Some(info) => println("line=" + info.line.toString()),
            None => println("none")
        }
        println(lookup(values, 9));
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "line=5\nafter\n");
}

/// Perspective 28. Sites do not bleed between calls: two faults raised from two
/// different helpers each report their own line, and a successful call in
/// between does not leave its location behind.
#[test]
fn prr_28_each_fault_reports_its_own_helper() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn first(values: Array<i64>) -> i64 {
    return values[9];
}

fn ok_call(values: Array<i64>) -> i64 {
    return values[0];
}

fn second(values: Array<i64>) -> i64 {
    return values[8];
}

fn guarded(values: Array<i64>, which: i64) -> i64 {
    let mut line = -1;
    if true {
        defer match recover() {
            Some(info) => { line = info.line; },
            None => {}
        }
        if which == 1 {
            return first(values);
        }
        return second(values);
    }
    return line;
}

fn main() {
    let values: Array<i64> = [1, 2, 3];
    println(guarded(values, 1).toString());
    println(ok_call(values).toString());
    println(guarded(values, 2).toString());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n1\n13\n");
}
