use super::support::*;

// Synchronous lexical panic/recover — willow-s9ej.3. These perspectives pin
// recovery capability, exactly-once defer consumption, lexical continuation,
// nested-panic behavior, and the Stage-2 async/cross-call boundary.

#[test]
fn panic_recover_01_nested_if_resumes_after_recovered_scope() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println("A");
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => {}
        }
        println("B");
        panic("boom");
        println("unreachable");
    }
    println("D");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "A\nB\nboom\nD\n");
}

#[test]
fn panic_recover_02_lifo_continues_with_older_defers_after_recovery() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer println("oldest");
        defer match recover() {
            Some(_) => println("recovered"),
            None => {}
        }
        defer println("newest");
        panic("boom");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "newest\nrecovered\noldest\nafter\n");
}

#[test]
fn panic_recover_03_normal_deferred_recover_is_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(_) => println("bad"),
            None => println("none")
        }
        println("body");
    }
    match recover() {
        Some(_) => println("bad-outside"),
        None => println("outside-none")
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "body\nnone\noutside-none\n");
}

#[test]
fn panic_recover_04_same_panic_is_consumed_once() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer {
            match recover() {
                Some(info) => println(info.message),
                None => println("first-none")
            }
            match recover() {
                Some(_) => println("twice"),
                None => println("second-none")
            }
        }
        panic("once");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "once\nsecond-none\nafter\n");
}

#[test]
fn panic_recover_05_helper_does_not_inherit_capability() {
    let (out, ok) = compile_and_run(
        r#"
fn hidden() -> Option<PanicInfo> { return recover(); }
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer:" + info.message),
            None => {}
        }
        if true {
            defer match hidden() {
                Some(_) => println("helper-recovered"),
                None => println("helper-none")
            }
            panic("boom");
        }
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "helper-none\nouter:boom\nafter\n");
}

#[test]
fn panic_recover_06_lambda_does_not_inherit_capability() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer:" + info.message),
            None => {}
        }
        if true {
            defer {
                let hidden = || { return recover(); };
                match hidden() {
                    Some(_) => println("lambda-recovered"),
                    None => println("lambda-none")
                }
            }
            panic("boom");
        }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "lambda-none\nouter:boom\n");
}

#[test]
fn panic_recover_07_non_void_outer_scope_is_rejected() {
    assert_compile_error_contains(
        r#"
fn value() -> i64 {
    defer match recover() { Some(_) => {}, None => {} }
    panic("boom");
}
fn main() {}
"#,
        &[
            "E0905",
            "outermost recovery scope",
            "without a return value",
        ],
    );
}

#[test]
fn panic_recover_08_async_recovery_before_first_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => println("none")
        }
        panic("async-before");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "async-before\nafter\n");
}

#[test]
fn panic_recover_09_void_outer_scope_completes_function() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    defer match recover() {
        Some(_) => println("recovered"),
        None => {}
    }
    panic("boom");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "recovered\n");
}

#[test]
fn panic_recover_10_match_arm_continues_at_match_join() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    match 1 {
        1 => {
            defer match recover() {
                Some(_) => println("match-recovered"),
                None => {}
            }
            panic("boom");
        },
        _ => println("wrong")
    }
    println("after-match");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "match-recovered\nafter-match\n");
}

#[test]
fn panic_recover_11_loop_body_recovery_uses_back_edge() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for i in 0..3 {
        defer match recover() {
            Some(_) => println("loop-recovered"),
            None => {}
        }
        if i == 1 { panic("boom"); }
        println(i);
    }
    println("after-loop");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\nloop-recovered\n2\nafter-loop\n");
}

#[test]
fn panic_recover_12_nested_panic_abandons_current_defer_without_rerun() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer:" + info.message),
            None => {}
        }
        if true {
            defer match recover() {
                Some(info) => println("inner:" + info.message),
                None => {}
            }
            defer { panic("B"); }
            panic("A");
        }
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "inner:B\nouter:A\nafter\n");
}

#[test]
fn panic_recover_13_panic_info_survives_gc_stress_in_defer() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                let noise = "a" + "b" + "c";
                println(noise);
                println(info.message);
                println(info.line > 0);
            },
            None => {}
        }
        panic("rooted");
    }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "abc\nrooted\ntrue\n");
}

#[test]
fn panic_recover_14_return_question_break_continue_keep_defer_contract() {
    let (out, ok) = compile_and_run(
        r#"
fn early() -> i64 { defer println("return-defer"); return 7; }
fn main() {
    println(early());
    for i in 0..3 {
        defer println(i);
        if i == 0 { continue; }
        if i == 1 { break; }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "return-defer\n7\n0\n1\n");
}

#[test]
fn panic_recover_15_direct_discarded_recover_stays_rejected() {
    assert_compile_error_contains(
        "fn main() { defer recover(); }",
        &["E0905", "discards the panic metadata"],
    );
}

#[test]
fn panic_recover_16_later_unregistered_defer_is_not_run() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(_) => println("recovered"),
            None => {}
        }
        if true { panic("boom"); }
        defer println("registered-too-late");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "recovered\nafter\n");
}

#[test]
fn panic_recover_17_nested_non_void_scope_returns_after_recovery() {
    let (out, ok) = compile_and_run(
        r#"
fn value() -> i64 {
    if true {
        defer match recover() {
            Some(_) => println("contained"),
            None => {}
        }
        panic("boom");
    }
    return 42;
}
fn main() { println(value()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "contained\n42\n");
}

#[test]
fn panic_recover_18_path_dependent_root_depth_is_restored() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(_) => println("recovered"),
            None => {}
        }
        if true {
            let early = "early" + "-root";
            println(early);
            panic("boom");
        }
        let late = "late" + "-root";
        println(late);
    }
    let after = "after" + "-root";
    println(after);
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "early-root\nrecovered\nafter-root\n");
}

#[test]
fn panic_recover_19_deep_mixed_backend_call_skips_scalar_placeholder() {
    let (out, ok) = compile_and_run(
        r#"
fn deep() -> i64 { panic("deep"); return 99; }
fn middle() -> i64 { return deep() + 1; }
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(middle());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "deep\nafter\n");
}

#[test]
fn panic_recover_20_instance_method_propagates() {
    let (out, ok) = compile_and_run(
        r#"
class Worker { pub fn run() -> i64 { panic("method"); return 7; } }
fn main() {
    let worker = new Worker();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(worker.run());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "method\nafter\n");
}

#[test]
fn panic_recover_21_static_method_propagates() {
    let (out, ok) = compile_and_run(
        r#"
class Worker { pub static fn run() -> String { panic("static"); return "bad"; } }
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(Worker::run());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "static\nafter\n");
}

#[test]
fn panic_recover_22_constructor_propagates_before_object_use() {
    let (out, ok) = compile_and_run(
        r#"
class Worker {
    pub init(self) { panic("init"); }
    pub fn run() { println("must-not-run"); }
}
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let worker = new Worker();
        worker.run();
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "init\nafter\n");
}

#[test]
fn panic_recover_23_interface_dispatch_propagates() {
    let (out, ok) = compile_and_run(
        r#"
interface Work { fn run(self) -> i64; }
class Worker implements Work {
    pub fn run() -> i64 { panic("interface"); return 3; }
}
fn main() {
    let worker: Work = new Worker();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(worker.run());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "interface\nafter\n");
}

#[test]
fn panic_recover_24_function_value_lambda_propagates() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let work: fn() -> i64 = || { panic("lambda"); return 4; };
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(work());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "lambda\nafter\n");
}

#[test]
fn panic_recover_25_module_call_propagates() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                r#"import worker;
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(worker::run());
    }
    println("after");
}
"#,
            ),
            (
                "worker.wi",
                "module worker;\npub fn run() -> i64 { panic(\"module\"); return 8; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "module\nafter\n");
}

#[test]
fn panic_recover_26_unhandled_diagnostic_keeps_original_call_chain() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deep() { panic("unhandled"); }
fn middle() { deep(); }
fn main() { middle(); }
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("runtime panic: unhandled"), "{out}");
    assert!(out.contains("0: deep"), "{out}");
    assert!(out.contains("1: middle"), "{out}");
}

#[test]
fn panic_recover_27_callee_and_caller_gc_roots_balance_under_stress() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn deep(value: String) -> String {
    let local = value + "-callee";
    println(local);
    panic("gc-call");
    return local;
}
fn main() {
    let caller = "caller" + "-root";
    if true {
        defer match recover() {
            Some(info) => {
                let noise = "noise" + "-alloc";
                println(noise);
                println(info.message);
            },
            None => {}
        }
        println(deep(caller));
    }
    println(caller);
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "caller-root-callee\nnoise-alloc\ngc-call\ncaller-root\n"
    );
}

#[test]
fn panic_recover_28_recovered_argument_panic_leaves_no_stale_call_frame() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn bad() -> i64 { panic("first"); return 0; }
class Worker { pub fn run(value: i64) { println(value); } }
fn main() {
    let worker = new Worker();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        worker.run(bad());
    }
    panic("second");
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("first"), "{out}");
    assert!(out.contains("runtime panic: second"), "{out}");
    assert!(!out.contains("0: run"), "stale recovered frame: {out}");
}

#[test]
fn panic_recover_29_recovered_reference_argument_context_is_cleared() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn bad() -> i64 { panic("first"); return 0; }
fn write(value: &mut i64, other: i64) { value = other; }
fn main() {
    let mut value = 1;
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        write(&value, bad());
    }
    panic("second");
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("first"), "{out}");
    assert!(out.contains("runtime panic: second"), "{out}");
    assert!(
        !out.contains("reference call:"),
        "stale reference context: {out}"
    );
}

#[test]
fn panic_recover_30_a_deep_panic_recovers_under_gc_stress() {
    let source = r#"
fn deep(value: String) -> String {
    let local = value + "-deep";
    panic("mixed");
    return local;
}
fn middle(value: String) -> String { return deep(value) + "-unused"; }
fn main() {
    let root = "caller" + "-root";
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(middle(root));
    }
    println(root);
}
"#;
    let (out, ok) = compile_with_env_and_run(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "run failed under GC stress: {out}");
    assert_eq!(out, "mixed\ncaller-root\n");
}

// Recoverable runtime language faults — willow-s9ej.5. Every helper returns a
// neutral ABI value after raising; these tests prove generated code branches
// before that placeholder can be printed, stored, or dereferenced.

#[test]
fn panic_recover_31_array_read_bounds_skips_neutral_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let values = [10];
        println(values[4]);
        println("unreachable");
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "array index out of bounds: the length is 1 but the index is 4\nafter\n"
    );
}

#[test]
fn panic_recover_32_array_write_bounds_does_not_store() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let values = [10];
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        values[3] = 99;
    }
    println(values[0]);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "array index out of bounds: the length is 1 but the index is 3\n10\n"
    );
}

#[test]
fn panic_recover_33_empty_array_pop_skips_zero_placeholder() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let values: Array<i64> = [];
        println(values.pop());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cannot pop from an empty array\nafter\n");
}

#[test]
fn panic_recover_34_integer_division_by_zero_is_recoverable() {
    let source = r#"
fn divide(value: i64) -> i64 { return 100 / value; }
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(divide(0));
    }
    println("after");
}
"#;
    let (out, ok) = compile_with_env_and_run(source, &[]);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "division by zero\nafter\n");
}

#[test]
fn panic_recover_35_integer_remainder_overflow_is_recoverable() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let minimum = 0 - 9223372036854775807 - 1;
        println(minimum % -1);
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "integer overflow: `i64::MIN % -1`\nafter\n");
}

#[test]
fn panic_recover_36_option_unwrap_none_is_recoverable() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let value: Option<String> = Option::None;
        println(value.unwrap());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "called `Option::unwrap()` on a `None` value\nafter\n");
}

#[test]
fn panic_recover_37_result_expect_err_uses_custom_message_under_gc_stress() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => println(info.message + "-handled"),
            None => {}
        }
        let value: Result<String, String> = Result::Err("bad");
        println(value.expect("custom" + " message"));
    }
    println("after");
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "custom message-handled\nafter\n");
}

#[test]
fn panic_recover_38_invalid_bounded_channel_capacity_is_recoverable() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let channel = Channel<i64>::with_capacity(0);
        channel.send(1);
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "channel capacity must be positive (rendezvous channels are not supported)\nafter\n"
    );
}

#[test]
fn panic_recover_39_sync_channel_would_block_is_recoverable() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        channel.send(2);
    }
    println(channel.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "send on full bounded channel would block\n1\n");
}

#[test]
fn panic_recover_40_runtime_fault_preserves_outer_panic_during_defer() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer:" + info.message),
            None => {}
        }
        defer {
            let values: Array<i64> = [];
            println(values.pop());
        }
        panic("original");
    }
    println("after");
}
"#,
    );
    assert!(!ok, "original panic must remain unhandled: {out}");
    assert!(
        out.contains("outer:cannot pop from an empty array"),
        "{out}"
    );
    assert!(out.contains("runtime panic: original"), "{out}");
}

#[test]
fn panic_recover_41_closed_empty_channel_recv_is_recoverable() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn main() {
    let channel = Channel<i64>::new();
    channel.close();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(channel.recv());
    }
    println("after");
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert!(out.starts_with("recv on closed empty channel\n"), "{out}");
    assert!(out.ends_with("after\n"), "{out}");
}

#[test]
fn panic_recover_42_async_recovery_after_resume_keeps_task_running() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn work() -> i64 {
    await sleep(1);
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        panic("after-resume");
    }
    println("still-running");
    return 42;
}
async fn main() { println(await work()); }
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "after-resume\nstill-running\n42\n");
}

#[test]
fn panic_recover_43_async_scope_recovers_sync_callee_panic() {
    let (out, ok) = compile_and_run(
        r#"
fn fail() -> String { panic("sync-callee"); return "neutral"; }
async fn main() {
    await sleep(0);
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(fail());
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sync-callee\nafter\n");
}

#[test]
fn panic_recover_44_async_match_arm_has_lexical_recovery_continuation() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    await sleep(0);
    match 1 {
        1 => {
            defer match recover() { Some(info) => println(info.message), None => {} }
            panic("match-async");
        },
        _ => println("wrong")
    }
    println("after-match");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "match-async\nafter-match\n");
}

#[test]
fn panic_recover_45_strict_cancelled_await_can_be_recovered() {
    let (out, ok) = compile_and_run(
        r#"
async fn waiting() -> i64 { await sleep(10000); return 7; }
async fn main() {
    let task = waiting();
    task.cancel();
    await sleep(1);
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(await task);
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert!(out.starts_with("awaited a cancelled task (task "), "{out}");
    assert!(out.ends_with("after\n"), "{out}");
}

#[test]
fn panic_recover_46_task_result_cancellation_remains_an_err_not_a_panic() {
    let (out, ok) = compile_and_run(
        r#"
async fn waiting() -> i64 { await sleep(10000); return 7; }
async fn main() {
    let task = waiting();
    task.cancel();
    await sleep(1);
    match await task.result() {
        Ok(value) => println(value),
        Err(Cancelled) => println("cancelled")
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\n");
}

#[test]
fn panic_recover_47_cancellation_only_defer_observes_none() {
    let (out, ok) = compile_and_run(
        r#"
async fn waiting() {
    defer match recover() {
        Some(_) => println("wrong"),
        None => println("cancel-none")
    }
    await sleep(10000);
}
async fn main() {
    let task = waiting();
    await sleep(1);
    task.cancel();
    await sleep(1);
    println(task.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancel-none\ntrue\n");
}

#[test]
fn panic_recover_48_older_cancel_defer_recovers_newer_cleanup_panic_once() {
    let (out, ok) = compile_and_run(
        r#"
async fn waiting() {
    defer match recover() {
        Some(info) => println("recovered:" + info.message),
        None => println("none")
    }
    defer panic("cancel-cleanup");
    await sleep(10000);
}
async fn main() {
    let task = waiting();
    await sleep(1);
    task.cancel();
    await sleep(1);
    task.cancel();
    await sleep(1);
    println(task.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "recovered:cancel-cleanup\ntrue\n");
}

#[test]
fn panic_recover_49_defer_rejects_scheduler_driving_channel_operation() {
    assert_compile_error_contains(
        r#"
fn main() {
    let channel = Channel<i64>::new();
    defer { println(channel.recv()); }
}
"#,
        &[
            "E0905",
            "scheduler-driving operations are not allowed inside a `defer`",
            "Channel.recv",
        ],
    );
}

#[test]
fn panic_recover_50_unhandled_async_panic_reports_payload_and_chain() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn deep() { await sleep(0); panic("async-unhandled"); }
async fn middle() { await deep(); }
async fn main() { await middle(); }
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("runtime panic: async-unhandled"), "{out}");
    assert!(out.contains("async stack (current task first)"), "{out}");
    assert!(out.contains("deep"), "{out}");
}

#[test]
fn panic_recover_51_unhandled_cancellation_cleanup_panic_runs_older_defer() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn waiting() {
    defer println("older-cleanup");
    defer panic("cancel-unhandled");
    await sleep(10000);
}
async fn main() {
    let task = waiting();
    await sleep(1);
    task.cancel();
    await sleep(10);
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("older-cleanup"), "{out}");
    assert!(out.contains("runtime panic: cancel-unhandled"), "{out}");
}

#[test]
fn panic_recover_52_async_recovered_panic_info_is_gc_safe() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn main() {
    await sleep(0);
    if true {
        defer match recover() {
            Some(info) => {
                let noise = "async" + "-noise";
                println(noise);
                println(info.message);
                println(info.line > 0);
            },
            None => {}
        }
        panic("async-rooted");
    }
    println("after");
}
"#,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "async-noise\nasync-rooted\ntrue\nafter\n");
}

#[test]
fn panic_recover_53_outermost_void_async_recovery_completes_task() {
    let (out, ok) = compile_and_run(
        r#"
async fn work() {
    defer match recover() { Some(info) => println(info.message), None => {} }
    await sleep(0);
    panic("outer-async");
}
async fn main() { await work(); println("completed"); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "outer-async\ncompleted\n");
}

#[test]
fn panic_recover_54_consumed_async_defer_is_not_rerun_by_later_cancel() {
    let (out, ok) = compile_and_run(
        r#"
async fn work() {
    if true {
        defer match recover() { Some(_) => println("once"), None => println("wrong") }
        panic("handled");
    }
    await sleep(10000);
}
async fn main() {
    let task = work();
    await sleep(1);
    task.cancel();
    await sleep(1);
    println(task.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "once\ntrue\n");
}
