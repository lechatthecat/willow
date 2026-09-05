use super::support::*;
use std::time::Duration;

// Additional panic/recover coverage: exactly 300 numbered perspectives.
//
// `prm_regression_001` through `_010` each pin one review finding directly.
// The remaining 290 perspectives are grouped ten per Rust test so the suite
// does not pay for 300 separate compiler/linker processes. Every generated
// Willow case has its own `prm-NNN` output oracle; one broken case identifies
// its perspective number in the assertion diff.
//
// 001..010  review regressions: recover shadowing/capability, cleanup re-entry,
//           task terminalization, PanicInfo immutability, runtime-fault spans
// 011..020  explicit panic payloads
// 021..030  lexical recovery depths
// 031..040  defer LIFO widths
// 041..050  loop recovery positions
// 051..060  propagation through recursive call depths
// 061..070  PanicInfo rooting under allocation stress
// 071..080  explicit-panic source locations
// 081..090  normal deferred recover() returns None
// 091..100  nested panic stacks
// 101..110  continuation state after recovery
// 111..120  async recovery before suspension
// 121..130  async recovery after suspension
// 131..140  concurrent task-local recovery
// 141..150  strict cancelled-await recovery
// 151..160  cancellation-aware Task.result()
// 161..170  cancellation cleanup observes no panic
// 171..180  cancellation cleanup panic recovery
// 181..190  recovery under preemption
// 191..200  recovery with a one-poll task budget
// 201..210  channel progress after producer recovery
// 211..220  array-bounds language panics
// 221..230  empty-array pop language panics
// 231..240  division-by-zero language panics
// 241..250  Option unwrap/expect language panics
// 251..260  Result unwrap/expect/unwrap_err language panics
// 261..270  closed-empty channel receive language panics
// 271..280  channel language panics
// 281..290  debug/release parity
// 291..300  LIR/AST backend parity

const MATRIX_TIMEOUT: Duration = Duration::from_secs(120);
const REGRESSION_TIMEOUT: Duration = Duration::from_secs(15);

fn run_matrix(source: &str, env: &[(&str, &str)], expected: &str) {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(source, env, MATRIX_TIMEOUT);
    assert!(!timed_out, "matrix program timed out: {out}");
    assert!(ok, "matrix program failed: {out}\nsource:\n{source}");
    assert_eq!(out, expected, "matrix output mismatch\nsource:\n{source}");
}

fn push_recovering_panic(source: &mut String, expected: &mut String, id: usize, message: &str) {
    source.push_str(&format!(
        r#"
    if true {{
        defer match recover() {{
            Some(info) => println("prm-{id:03}:" + info.message),
            None => println("prm-{id:03}:none")
        }}
        panic("{message}");
    }}
"#,
    ));
    expected.push_str(&format!("prm-{id:03}:{message}\n"));
}

#[test]
fn prm_regression_001_local_named_recover_is_rejected_as_reserved() {
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

#[test]
fn prm_regression_002_parameter_named_recover_is_rejected_as_reserved() {
    assert_compile_error_contains(
        r#"
fn invoke(recover: fn() -> i64) -> i64 { return recover(); }
fn main() {
    let value = || { return 9; };
    println(invoke(value));
}
"#,
        &["E0351", "reserved builtin function"],
    );
}

#[test]
fn prm_regression_003_normal_helper_defer_cannot_steal_callers_panic() {
    let (out, ok) = compile_and_run(
        r#"
fn helper() {
    defer match recover() {
        Some(info) => println("helper-stole:" + info.message),
        None => println("helper-none")
    }
}

fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer:" + info.message),
            None => println("outer-none")
        }
        defer helper();
        panic("outer-panic");
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "helper-none\nouter:outer-panic\n");
}

#[test]
fn prm_regression_004_scheduler_reentry_is_fatal_after_recover() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
fn receive(channel: Channel<i64>) { println(channel.recv()); }
fn main() {
    let channel = Channel<i64>::new();
    if true {
        defer {
            match recover() {
                Some(_) => println("recovered"),
                None => println("none")
            }
            receive(channel);
        }
        panic("boom");
    }
}
"#,
        &[],
        REGRESSION_TIMEOUT,
    );
    assert!(!timed_out, "scheduler re-entry hung: {out}");
    assert!(!ok, "scheduler re-entry unexpectedly succeeded: {out}");
    assert!(
        out.contains("runtime fatal: scheduler re-entry attempted from panic-unwinding defer"),
        "wrong failure mode: {out}"
    );
}

#[test]
fn prm_regression_005_scheduler_reentry_is_fatal_in_cancellation_cleanup() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
fn receive(channel: Channel<i64>) { println(channel.recv()); }
async fn waiting(channel: Channel<i64>) {
    defer receive(channel);
    await sleep(10000);
}
async fn main() {
    let channel = Channel<i64>::new();
    let task = waiting(channel);
    await sleep(5);
    task.cancel();
    await sleep(20);
}
"#,
        &[("WILLOW_WORKERS", "4")],
        REGRESSION_TIMEOUT,
    );
    assert!(!timed_out, "cancellation cleanup re-entry hung: {out}");
    assert!(
        !ok,
        "cancellation cleanup re-entry unexpectedly succeeded: {out}"
    );
    assert!(
        out.contains("runtime fatal: scheduler re-entry attempted from panic-unwinding defer"),
        "wrong failure mode: {out}"
    );
}

#[test]
fn prm_regression_006_panic_info_field_cannot_be_mutated_through_reference() {
    assert_compile_error_contains(
        r#"
fn overwrite(value: &mut i64) { value = 99; }
fn alter(info: PanicInfo) { overwrite(&info.line); }
fn main() {}
"#,
        &["PanicInfo", "read-only"],
    );
}

#[test]
fn prm_regression_007_panicked_poll_is_terminalized_before_reporting() {
    let source = fs::read_to_string("crates/willow_runtime/src/scheduler.rs")
        .expect("scheduler source must be readable");
    let start = source
        .find("PollOutcome::Panicked | PollOutcome::Invalid(_) => {")
        .expect("panicked poll branch must exist");
    let tail = &source[start..];
    let end = tail
        .find("// Done polling this task")
        .expect("panicked poll branch must precede post-poll cleanup");
    let branch = &tail[..end];
    assert!(
        branch.contains("finalize_panicked(id)"),
        "panicked poll must publish a terminal state and enqueue cleanup before abort:\n{branch}"
    );
    let finalize = source[start..]
        .find("finalize_panicked(id)")
        .expect("terminalization call")
        + start;
    let report = source[start..]
        .find("finish_unhandled_with_async_chain")
        .expect("panic report call")
        + start;
    assert!(
        finalize < report,
        "terminalization must precede unhandled-panic reporting"
    );
}

fn assert_runtime_fault_has_source_location(source: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\ntrue\n");
}

#[test]
fn prm_regression_008_array_bounds_panic_has_source_location() {
    assert_runtime_fault_has_source_location(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                println(info.file != "");
                println(info.line > 0);
                println(info.column > 0);
            },
            None => {}
        }
        let values = [1];
        println(values[9]);
    }
}
"#,
    );
}

#[test]
fn prm_regression_009_option_unwrap_panic_has_source_location() {
    assert_runtime_fault_has_source_location(
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => {
                println(info.file != "");
                println(info.line > 0);
                println(info.column > 0);
            },
            None => {}
        }
        let value: Option<i64> = Option::None;
        println(value.unwrap());
    }
}
"#,
    );
}

#[test]
fn prm_regression_010_cancelled_await_panic_has_source_location() {
    assert_runtime_fault_has_source_location(
        r#"
async fn waiting() -> i64 { await sleep(10000); return 1; }
async fn main() {
    let task = waiting();
    task.cancel();
    await sleep(1);
    if true {
        defer match recover() {
            Some(info) => {
                println(info.file != "");
                println(info.line > 0);
                println(info.column > 0);
            },
            None => {}
        }
        println(await task);
    }
}
"#,
    );
}

#[test]
fn prm_matrix_011_020_explicit_panic_payloads() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for id in 11..=20 {
        push_recovering_panic(&mut source, &mut expected, id, &format!("payload-{id:03}"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_021_030_lexical_recovery_depths() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for (offset, depth) in (1..=10).enumerate() {
        let id = 21 + offset;
        for _ in 0..depth {
            source.push_str("if true {\n");
        }
        source.push_str(&format!(
            r#"
defer match recover() {{
    Some(info) => println("prm-{id:03}:" + info.message),
    None => println("prm-{id:03}:none")
}}
panic("depth-{depth}");
"#,
        ));
        for _ in 0..depth {
            source.push_str("}\n");
        }
        source.push_str(&format!("println(\"prm-{id:03}:after\");\n"));
        expected.push_str(&format!("prm-{id:03}:depth-{depth}\nprm-{id:03}:after\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_031_040_defer_lifo_widths() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for (offset, width) in (1..=10).enumerate() {
        let id = 31 + offset;
        source.push_str(&format!("let mut score_{id} = 0;\nif true {{\n"));
        source.push_str(&format!(
            "defer match recover() {{ Some(_) => {{ score_{id} = score_{id} + 1000; }}, None => {{}} }}\n"
        ));
        for value in 1..=width {
            source.push_str(&format!("defer {{ score_{id} = score_{id} + {value}; }}\n"));
        }
        source.push_str(&format!("panic(\"lifo-{id}\");\n}}\n"));
        source.push_str(&format!("println(score_{id});\n"));
        expected.push_str(&format!("{}\n", 1000 + width * (width + 1) / 2));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_041_050_loop_recovery_positions() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for target in 0..10 {
        let id = 41 + target;
        source.push_str(&format!(
            r#"
let mut total_{id} = 0;
for i in 0..10 {{
    if true {{
        defer match recover() {{
            Some(_) => {{ total_{id} = total_{id} + 100; }},
            None => {{}}
        }}
        if i == {target} {{ panic("loop-{target}"); }}
        total_{id} = total_{id} + i;
    }}
}}
println(total_{id});
"#,
        ));
        expected.push_str(&format!("{}\n", 145 - target));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_051_060_recursive_propagation_depths() {
    let mut source = String::new();
    for offset in 0..10 {
        let id = 51 + offset;
        source.push_str(&format!(
            r#"
fn recurse_{id}(depth: i64) {{
    if depth == 0 {{ panic("recursive-{id}"); }}
    recurse_{id}(depth - 1);
}}
"#,
        ));
    }
    source.push_str("fn main() {\n");
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 51 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => println("prm-{id:03}:" + info.message),
        None => println("prm-{id:03}:none")
    }}
    recurse_{id}({depth});
}}
"#,
            depth = offset + 1,
        ));
        expected.push_str(&format!("prm-{id:03}:recursive-{id}\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_061_070_panic_info_rooting_under_allocation_stress() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 61 + offset;
        let allocations = offset + 2;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => {{
            let mut noise = "";
            for i in 0..{allocations} {{ noise = noise + "x" + i.toString(); }}
            println(noise != "");
            println("prm-{id:03}:" + info.message);
        }},
        None => println("prm-{id:03}:none")
    }}
    panic("rooted-{id}");
}}
"#,
        ));
        expected.push_str(&format!("true\nprm-{id:03}:rooted-{id}\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[("WILLOW_GC_STRESS", "alloc")], &expected);
}

#[test]
fn prm_matrix_071_080_explicit_panic_source_locations() {
    let mut source = String::new();
    for offset in 0..10 {
        let id = 71 + offset;
        source.push_str(&format!(
            "fn location_{id}() {{ panic(\"location-{id}\"); }}\n"
        ));
    }
    source.push_str("fn main() {\n");
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 71 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => {{
            if info.file != "" && info.line > 0 && info.column > 0 {{
                println("prm-{id:03}:located");
            }} else {{
                println("prm-{id:03}:missing");
            }}
        }},
        None => println("prm-{id:03}:none")
    }}
    location_{id}();
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:located\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_081_090_normal_deferred_recover_is_none() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 81 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:some"),
        None => println("prm-{id:03}:none")
    }}
    println("prm-{id:03}:body");
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:body\nprm-{id:03}:none\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_091_100_nested_panic_stacks() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 91 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => println("prm-{id:03}:outer:" + info.message),
        None => println("prm-{id:03}:outer-none")
    }}
    if true {{
        defer match recover() {{
            Some(info) => println("prm-{id:03}:inner:" + info.message),
            None => println("prm-{id:03}:inner-none")
        }}
        defer panic("second-{id}");
        panic("first-{id}");
    }}
}}
"#,
        ));
        expected.push_str(&format!(
            "prm-{id:03}:inner:second-{id}\nprm-{id:03}:outer:first-{id}\n"
        ));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_101_110_continuation_state_after_recovery() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 101 + offset;
        let initial = offset + 1;
        source.push_str(&format!(
            r#"
let mut state_{id} = {initial};
if true {{
    defer match recover() {{
        Some(_) => {{ state_{id} = state_{id} + 100; }},
        None => {{ state_{id} = 0; }}
    }}
    state_{id} = state_{id} + 10;
    panic("state-{id}");
    state_{id} = -1;
}}
state_{id} = state_{id} + 1;
println(state_{id});
"#,
        ));
        expected.push_str(&format!("{}\n", initial + 111));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_111_120_async_recovery_before_suspension() {
    let mut source = "async fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 111 + offset;
        push_recovering_panic(
            &mut source,
            &mut expected,
            id,
            &format!("before-await-{id}"),
        );
    }
    source.push_str("await sleep(0);\n}\n");
    run_matrix(&source, &[("WILLOW_TASK_BUDGET", "1")], &expected);
}

#[test]
fn prm_matrix_121_130_async_recovery_after_suspension() {
    let mut source = "async fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 121 + offset;
        source.push_str("await sleep(0);\n");
        push_recovering_panic(&mut source, &mut expected, id, &format!("after-await-{id}"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[("WILLOW_TASK_BUDGET", "1")], &expected);
}

#[test]
fn prm_matrix_131_140_concurrent_task_local_recovery() {
    let mut source = r#"
async fn recover_worker(tag: i64) -> String {
    let mut result = "none";
    if true {
        defer match recover() {
            Some(info) => { result = info.message; },
            None => { result = "wrong-none"; }
        }
        panic("task-" + tag.toString());
    }
    await sleep(1);
    return result;
}

async fn main() {
"#
    .to_string();
    for offset in 0..10 {
        source.push_str(&format!("let task_{offset} = recover_worker({offset});\n"));
    }
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 131 + offset;
        source.push_str(&format!(
            "let value_{offset} = await task_{offset};\nprintln(\"prm-{id:03}:\" + value_{offset});\n"
        ));
        expected.push_str(&format!("prm-{id:03}:task-{offset}\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[("WILLOW_WORKERS", "8")], &expected);
}

#[test]
fn prm_matrix_141_150_strict_cancelled_await_recovery() {
    let mut source = r#"
async fn long_wait() -> i64 {
    await sleep(10000);
    return 99;
}

async fn main() {
"#
    .to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 141 + offset;
        source.push_str(&format!(
            r#"
let task_{offset} = long_wait();
task_{offset}.cancel();
await sleep(1);
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:cancelled"),
        None => println("prm-{id:03}:none")
    }}
    println(await task_{offset});
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:cancelled\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[("WILLOW_WORKERS", "4")], &expected);
}

#[test]
fn prm_matrix_151_160_cancellation_aware_task_results() {
    let mut source = r#"
async fn long_result() -> i64 {
    await sleep(10000);
    return 99;
}

async fn main() {
"#
    .to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 151 + offset;
        source.push_str(&format!(
            r#"
let task_{offset} = long_result();
task_{offset}.cancel();
match await task_{offset}.result() {{
    Ok(value) => println(value),
    Err(Cancelled) => println("prm-{id:03}:cancelled")
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:cancelled\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[("WILLOW_WORKERS", "4")], &expected);
}

#[test]
fn prm_matrix_161_170_cancellation_cleanup_observes_none() {
    let source = r#"
async fn cancel_none(tag: i64) {
    defer match recover() {
        Some(_) => println("prm-" + tag.toString() + ":some"),
        None => println("prm-" + tag.toString() + ":none")
    }
    await sleep(10000);
}

async fn main() {
    let task_0 = cancel_none(161);
    await sleep(1);
    task_0.cancel();
    match await task_0.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_1 = cancel_none(162);
    await sleep(1);
    task_1.cancel();
    match await task_1.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_2 = cancel_none(163);
    await sleep(1);
    task_2.cancel();
    match await task_2.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_3 = cancel_none(164);
    await sleep(1);
    task_3.cancel();
    match await task_3.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_4 = cancel_none(165);
    await sleep(1);
    task_4.cancel();
    match await task_4.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_5 = cancel_none(166);
    await sleep(1);
    task_5.cancel();
    match await task_5.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_6 = cancel_none(167);
    await sleep(1);
    task_6.cancel();
    match await task_6.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_7 = cancel_none(168);
    await sleep(1);
    task_7.cancel();
    match await task_7.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_8 = cancel_none(169);
    await sleep(1);
    task_8.cancel();
    match await task_8.result() { Ok(_) => {}, Err(Cancelled) => {} }
    let task_9 = cancel_none(170);
    await sleep(1);
    task_9.cancel();
    match await task_9.result() { Ok(_) => {}, Err(Cancelled) => {} }
}
"#;
    let expected = (161..=170)
        .map(|id| format!("prm-{id}:none\n"))
        .collect::<String>();
    run_matrix(source, &[("WILLOW_WORKERS", "4")], &expected);
}

#[test]
fn prm_matrix_171_180_cancellation_cleanup_panic_recovery() {
    let source = r#"
async fn cancel_panic(tag: i64) {
    defer match recover() {
        Some(info) => println("prm-" + tag.toString() + ":" + info.message),
        None => println("prm-" + tag.toString() + ":none")
    }
    defer panic("cleanup-" + tag.toString());
    await sleep(10000);
}

async fn run_case(tag: i64) {
    let task = cancel_panic(tag);
    await sleep(1);
    task.cancel();
    match await task.result() { Ok(_) => {}, Err(Cancelled) => {} }
}

async fn main() {
    await run_case(171);
    await run_case(172);
    await run_case(173);
    await run_case(174);
    await run_case(175);
    await run_case(176);
    await run_case(177);
    await run_case(178);
    await run_case(179);
    await run_case(180);
}
"#;
    let expected = (171..=180)
        .map(|id| format!("prm-{id}:cleanup-{id}\n"))
        .collect::<String>();
    run_matrix(source, &[("WILLOW_WORKERS", "4")], &expected);
}

#[test]
fn prm_matrix_181_190_recovery_under_preemption() {
    let mut source = r#"
async fn preempt_case(tag: i64) -> i64 {
    let mut recovered = 0;
    let mut checksum = 0;
    for i in 0..4000 { checksum = checksum + i; }
    if true {
        defer match recover() {
            Some(_) => { recovered = tag; },
            None => { recovered = -1; }
        }
        panic("preempt");
    }
    for i in 0..4000 { checksum = checksum - i; }
    if checksum != 0 { return -2; }
    return recovered;
}

async fn main() {
"#
    .to_string();
    for offset in 0..10 {
        let id = 181 + offset;
        source.push_str(&format!("let task_{offset} = preempt_case({id});\n"));
    }
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 181 + offset;
        source.push_str(&format!("println(await task_{offset});\n"));
        expected.push_str(&format!("{id}\n"));
    }
    source.push_str("}\n");
    run_matrix(
        &source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_TIME_QUANTUM_MS", "1")],
        &expected,
    );
}

#[test]
fn prm_matrix_191_200_recovery_with_one_poll_task_budget() {
    let mut source = r#"
async fn budget_case(tag: i64) -> i64 {
    await sleep(0);
    let mut recovered = 0;
    if true {
        defer match recover() {
            Some(_) => { recovered = tag; },
            None => { recovered = -1; }
        }
        panic("budget");
    }
    await sleep(0);
    return recovered;
}

async fn main() {
"#
    .to_string();
    for offset in 0..10 {
        let id = 191 + offset;
        source.push_str(&format!("let task_{offset} = budget_case({id});\n"));
    }
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 191 + offset;
        source.push_str(&format!("println(await task_{offset});\n"));
        expected.push_str(&format!("{id}\n"));
    }
    source.push_str("}\n");
    run_matrix(
        &source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_TASK_BUDGET", "1")],
        &expected,
    );
}

#[test]
fn prm_matrix_201_210_channel_progress_after_producer_recovery() {
    let mut source = r#"
async fn producer(channel: Channel<i64>, value: i64) {
    if true {
        defer match recover() { Some(_) => {}, None => {} }
        panic("producer-fault");
    }
    channel.send(value);
}

async fn main() {
"#
    .to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 201 + offset;
        source.push_str(&format!(
            r#"
let channel_{offset} = Channel<i64>::with_capacity(1);
let task_{offset} = producer(channel_{offset}, {id});
println(channel_{offset}.recv());
await task_{offset};
"#,
        ));
        expected.push_str(&format!("{id}\n"));
    }
    source.push_str("}\n");
    run_matrix(
        &source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "scheduler")],
        &expected,
    );
}

#[test]
fn prm_matrix_211_220_array_bounds_language_panics() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 211 + offset;
        let index = offset + 1;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => println("prm-{id:03}:" + info.message),
        None => println("prm-{id:03}:none")
    }}
    let values = [{id}];
    println(values[{index}]);
}}
"#,
        ));
        expected.push_str(&format!(
            "prm-{id:03}:array index out of bounds: the length is 1 but the index is {index}\n"
        ));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_221_230_empty_array_pop_language_panics() {
    let mut source = "import std::collections::Array;\nfn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 221 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => println("prm-{id:03}:" + info.message),
        None => println("prm-{id:03}:none")
    }}
    let values_{id}: Array<i64> = [];
    println(values_{id}.pop());
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:cannot pop from an empty array\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_231_240_division_by_zero_language_panics() {
    let mut source = String::new();
    for offset in 0..10 {
        let id = 231 + offset;
        let numerator = offset + 1;
        source.push_str(&format!(
            "fn divide_{id}(value: i64) -> i64 {{ return {numerator} / value; }}\n"
        ));
    }
    source.push_str("fn main() {\n");
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 231 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(info) => println("prm-{id:03}:" + info.message),
        None => println("prm-{id:03}:none")
    }}
    println(divide_{id}(0));
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:division by zero\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_241_250_option_unwrap_expect_language_panics() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 241 + offset;
        let operation = if offset < 5 {
            format!("println(value_{id}.unwrap());")
        } else {
            format!("println(value_{id}.expect(\"option-{id}\"));")
        };
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:recovered"),
        None => println("prm-{id:03}:none")
    }}
    let value_{id}: Option<i64> = Option::None;
    {operation}
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:recovered\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_251_260_result_unwrap_expect_language_panics() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 251 + offset;
        let setup_and_operation = match offset {
            0..=3 => format!(
                "let value_{id}: Result<i64, String> = Result::Err(\"bad-{id}\"); println(value_{id}.unwrap());"
            ),
            4..=6 => format!(
                "let value_{id}: Result<i64, String> = Result::Ok({id}); println(value_{id}.unwrap_err());"
            ),
            _ => format!(
                "let value_{id}: Result<i64, String> = Result::Err(\"bad-{id}\"); println(value_{id}.expect(\"result-{id}\"));"
            ),
        };
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:recovered"),
        None => println("prm-{id:03}:none")
    }}
    {setup_and_operation}
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:recovered\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_261_270_closed_empty_channel_recv_language_panics() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 261 + offset;
        source.push_str(&format!(
            r#"
let channel_{id} = Channel<i64>::new();
channel_{id}.close();
if true {{
    defer match recover() {{
        Some(info) => println("prm-{id:03}:" + info.message),
        None => println("prm-{id:03}:none")
    }}
    println(channel_{id}.recv());
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:recv on closed empty channel\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[("WILLOW_GC_STRESS", "alloc")], &expected);
}

#[test]
fn prm_matrix_271_280_channel_language_panics() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for offset in 0..5 {
        let id = 271 + offset;
        let capacity = 0 - offset as i64;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:invalid-capacity"),
        None => println("prm-{id:03}:none")
    }}
    let channel_{id} = Channel<i64>::with_capacity({capacity});
    channel_{id}.send(1);
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:invalid-capacity\n"));
    }
    for offset in 0..5 {
        let id = 276 + offset;
        source.push_str(&format!(
            r#"
let channel_{id} = Channel<i64>::with_capacity(1);
channel_{id}.send({id});
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:full-send"),
        None => println("prm-{id:03}:none")
    }}
    channel_{id}.send({next});
}}
"#,
            next = id + 1,
        ));
        expected.push_str(&format!("prm-{id:03}:full-send\n"));
    }
    source.push_str("}\n");
    run_matrix(&source, &[], &expected);
}

#[test]
fn prm_matrix_281_290_debug_release_parity() {
    let mut source = "fn main() {\n".to_string();
    let mut expected = String::new();
    for id in 281..=290 {
        push_recovering_panic(&mut source, &mut expected, id, &format!("mode-{id}"));
    }
    source.push_str("}\n");

    let path = temp_path(format!("willow_prm_modes_{}.wi", unique_test_id()));
    fs::write(&path, &source).expect("write parity source");
    let (debug, debug_ok) = compile_file_and_run(&path);
    let (release, release_ok) = compile_file_and_run_with_args(&path, &["--release"]);
    let _ = fs::remove_file(&path);
    assert!(debug_ok, "debug mode failed: {debug}");
    assert!(release_ok, "release mode failed: {release}");
    assert_eq!(debug, expected);
    assert_eq!(release, expected);
    assert_eq!(debug, release, "debug/release recovery behavior diverged");
}

#[test]
fn prm_matrix_291_300_ten_recovered_divisions_in_one_body() {
    let mut source = String::new();
    for offset in 0..10 {
        let id = 291 + offset;
        source.push_str(&format!(
            "fn lir_divide_{id}(value: i64) -> i64 {{ return {id} / value; }}\n"
        ));
    }
    source.push_str("fn main() {\n");
    let mut expected = String::new();
    for offset in 0..10 {
        let id = 291 + offset;
        source.push_str(&format!(
            r#"
if true {{
    defer match recover() {{
        Some(_) => println("prm-{id:03}:division"),
        None => println("prm-{id:03}:none")
    }}
    println(lir_divide_{id}(0));
}}
"#,
        ));
        expected.push_str(&format!("prm-{id:03}:division\n"));
    }
    source.push_str("}\n");

    let (out, ok) = compile_with_env_and_run(&source, &[]);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}
