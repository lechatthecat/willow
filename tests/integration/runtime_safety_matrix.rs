use super::support::*;
use std::time::Duration;

// Non-recovery regression coverage for the runtime/compiler changes currently
// in flight. These tests deliberately avoid recover(); runtime faults are
// observed only through process exit and diagnostics.

const ASYNC_TIMEOUT: Duration = Duration::from_secs(30);

fn compile_and_run_release_check_exit(source: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_exit_{id}.wi"));
    let bin_path = temp_path(format!("willow_release_exit_{id}"));
    fs::write(&src_path, source).expect("write release fixture");

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let compiled = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("compile release fixture");
    if !compiled.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&compiled.stdout),
            String::from_utf8_lossy(&compiled.stderr)
        );
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (combined, false);
    }

    let output = Command::new(&bin_path)
        .output()
        .expect("run release fixture");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
    (combined, output.status.success())
}

fn assert_unhandled_fault(source: &str, release: bool, expected: &str) -> String {
    let (out, ok) = if release {
        compile_and_run_release_check_exit(source)
    } else {
        compile_and_run_check_exit(source)
    };
    assert!(!ok, "expected runtime failure, got success: {out}");
    assert!(
        out.contains(expected),
        "failure did not contain `{expected}`: {out}"
    );
    out
}

fn assert_async_output(source: &str, env: &[(&str, &str)], expected: &str) {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(source, env, ASYNC_TIMEOUT);
    assert!(!timed_out, "async fixture timed out: {out}");
    assert!(ok, "async fixture failed: {out}\nsource:\n{source}");
    assert_eq!(out, expected);
}

const NIL_FIELD_SOURCE: &str = r#"
class NilNode { pub value: i64; }
fn main() {
    let channel = Channel<NilNode>::new();
    channel.close();
    let node = channel.recv();
    println(node.value);
}
"#;

#[test]
fn rsm_01_release_nil_field_access_fails() {
    let out = assert_unhandled_fault(NIL_FIELD_SOURCE, true, "nil dereference");
    assert!(out.contains(" at "), "missing source location: {out}");
}

#[test]
fn rsm_02_release_nil_method_call_fails() {
    let out = assert_unhandled_fault(
        r#"
class NilCounter {
    pub value: i64;
    pub fn read(self) -> i64 { return self.value; }
}
fn main() {
    let channel = Channel<NilCounter>::new();
    channel.close();
    let counter = channel.recv();
    println(counter.read());
}
"#,
        true,
        "nil dereference",
    );
    assert!(out.contains(" at "), "missing source location: {out}");
}

#[test]
fn rsm_03_release_nil_interface_dispatch_fails() {
    let out = assert_unhandled_fault(
        r#"
interface NilReader { fn read(self) -> i64; }
class NilReaderImpl implements NilReader {
    pub value: i64;
    pub fn read(self) -> i64 { return self.value; }
}
fn main() {
    let channel = Channel<NilReaderImpl>::new();
    channel.close();
    let concrete = channel.recv();
    let reader: NilReader = concrete;
    println(reader.read());
}
"#,
        true,
        "nil dereference",
    );
    assert!(out.contains(" at "), "missing source location: {out}");
}

#[test]
fn rsm_04_nil_diagnostic_has_debug_release_location_parity() {
    let debug = assert_unhandled_fault(NIL_FIELD_SOURCE, false, "nil dereference");
    let release = assert_unhandled_fault(NIL_FIELD_SOURCE, true, "nil dereference");
    for (mode, out) in [("debug", &debug), ("release", &release)] {
        assert!(
            out.contains(" at "),
            "{mode} missing source location: {out}"
        );
        assert!(out.contains(".wi:"), "{mode} missing source file: {out}");
    }
    assert!(
        debug.contains("`value`"),
        "debug mode should retain receiver context: {debug}"
    );
}

#[test]
fn rsm_05_select_recv_arm_flushes_lexical_defer() {
    assert_async_output(
        r#"
async fn feed(channel: Channel<i64>) { await sleep(1); channel.send(7); }
async fn main() {
    let channel = Channel<i64>::new();
    let feeder = feed(channel);
    select {
        let value = channel.recv() => {
            defer println("recv-defer");
            println(value);
        }
        sleep(5000) => { println("timeout"); }
    }
    await feeder;
    println("after");
}
"#,
        &[],
        "7\nrecv-defer\nafter\n",
    );
}

#[test]
fn rsm_06_select_send_arm_flushes_lexical_defer() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<i64>::with_capacity(1);
    select {
        channel.send(8) => {
            defer println("send-defer");
            println("send-body");
        }
    }
    println(channel.recv());
}
"#,
        &[],
        "send-body\nsend-defer\n8\n",
    );
}

#[test]
fn rsm_07_select_timeout_arm_flushes_lexical_defer() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<i64>::new();
    select {
        let value = channel.recv() => { println(value); }
        sleep(1) => {
            defer println("timeout-defer");
            println("timeout-body");
        }
    }
    println("after");
}
"#,
        &[],
        "timeout-body\ntimeout-defer\nafter\n",
    );
}

#[test]
fn rsm_08_select_task_arm_flushes_lexical_defer() {
    assert_async_output(
        r#"
async fn value() -> i64 { await sleep(1); return 9; }
async fn main() {
    let task = value();
    select {
        let result = await task => {
            defer println("task-defer");
            println(result);
        }
        sleep(5000) => { println("timeout"); }
    }
    println("after");
}
"#,
        &[],
        "9\ntask-defer\nafter\n",
    );
}

#[test]
fn rsm_09_select_default_arm_flushes_lexical_defer() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<i64>::new();
    select {
        let value = channel.recv() => { println(value); }
        default => {
            defer println("default-defer");
            println("default-body");
        }
    }
    println("after");
}
"#,
        &[],
        "default-body\ndefault-defer\nafter\n",
    );
}

#[test]
fn rsm_10_non_selected_select_arm_does_not_register_defer() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(3);
    select {
        let value = channel.recv() => {
            defer println("selected-defer");
            println(value);
        }
        sleep(5000) => { defer println("wrong-timeout-defer"); }
    }
}
"#,
        &[],
        "3\nselected-defer\n",
    );
}

#[test]
fn rsm_11_select_string_binding_survives_gc_stress_until_defer() {
    assert_async_output(
        r#"
async fn feed(channel: Channel<String>) {
    await sleep(1);
    channel.send("sel" + "ect");
}
async fn main() {
    let channel = Channel<String>::new();
    let feeder = feed(channel);
    select {
        let value = channel.recv() => {
            defer println(value);
            let noise = "g" + "c";
            println(noise);
        }
        sleep(5000) => { println("timeout"); }
    }
    await feeder;
}
"#,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
        "gc\nselect\n",
    );
}

#[test]
fn rsm_12_cancel_inside_selected_arm_runs_pending_defer() {
    assert_async_output(
        r#"
async fn selected() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    select {
        let value = channel.recv() => {
            defer println("selected-cleanup");
            println(value);
            await sleep(10000);
        }
    }
}
async fn main() {
    let task = selected();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "1\nselected-cleanup\ntrue\n",
    );
}

#[test]
fn rsm_13_completed_select_arm_defer_is_not_rerun_on_cancel() {
    assert_async_output(
        r#"
async fn selected() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    select {
        let value = channel.recv() => {
            defer println("once");
            println(value);
        }
    }
    await sleep(10000);
}
async fn main() {
    let task = selected();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "1\nonce\ntrue\n",
    );
}

#[test]
fn rsm_14_while_normal_iterations_flush_per_iteration() {
    assert_async_output(
        r#"
fn show(value: i64) { println(value); }
async fn main() {
    let mut i = 0;
    while i < 3 {
        defer show(i);
        await sleep(0);
        i = i + 1;
    }
    println(9);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        "0\n1\n2\n9\n",
    );
}

#[test]
fn rsm_15_while_continue_flushes_current_iteration() {
    assert_async_output(
        r#"
fn show(value: i64) { println(value); }
async fn main() {
    let mut i = 0;
    while i < 3 {
        let current = i;
        i = i + 1;
        defer show(current);
        if current == 1 { continue; }
        println(7);
    }
}
"#,
        &[],
        "7\n0\n1\n7\n2\n",
    );
}

#[test]
fn rsm_16_while_break_flushes_before_outer_scope() {
    assert_async_output(
        r#"
fn show(value: i64) { println(value); }
async fn main() {
    defer show(9);
    let mut i = 0;
    while i < 3 {
        defer show(i);
        if i == 1 { break; }
        i = i + 1;
    }
    println(8);
}
"#,
        &[],
        "0\n1\n8\n9\n",
    );
}

#[test]
fn rsm_17_cancel_inside_while_runs_active_iteration_defer() {
    assert_async_output(
        r#"
async fn waiting() {
    let mut i = 0;
    while i < 2 {
        defer println("while-cleanup");
        await sleep(10000);
        i = i + 1;
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "while-cleanup\ntrue\n",
    );
}

#[test]
fn rsm_18_nested_while_scopes_unwind_inner_before_outer() {
    assert_async_output(
        r#"
fn show(value: i64) { println(value); }
async fn main() {
    let mut outer = 0;
    while outer < 2 {
        defer show(100 + outer);
        let mut inner = 0;
        while inner < 2 {
            defer show(10 * outer + inner);
            inner = inner + 1;
        }
        outer = outer + 1;
    }
}
"#,
        &[],
        "0\n1\n100\n10\n11\n101\n",
    );
}

#[test]
fn rsm_19_cancel_before_while_defer_registration_runs_nothing() {
    assert_async_output(
        r#"
async fn waiting() {
    let mut active = true;
    while active {
        await sleep(10000);
        defer println("not-registered");
        active = false;
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "true\n",
    );
}

#[test]
fn rsm_20_while_string_capture_survives_gc_stress() {
    assert_async_output(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        let text = "item-" + i.toString();
        defer println(text);
        let noise = "a" + "b";
        println(noise);
        await sleep(0);
        i = i + 1;
    }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
        "ab\nitem-0\nab\nitem-1\nab\nitem-2\n",
    );
}

fn null_i64_array_program(operation: &str) -> String {
    format!(
        r#"
import std::collections::Array;
fn nil_array() -> Array<i64> {{
    let channel = Channel<Array<i64>>::new();
    channel.close();
    return channel.recv();
}}
fn main() {{
    let values = nil_array();
    {operation}
}}
"#,
    )
}

#[test]
fn rsm_21_null_array_len_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values.len());"),
        false,
        "cannot take the length of a null array",
    );
}

#[test]
fn rsm_22_null_array_index_read_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values[0]);"),
        false,
        "cannot index a null array",
    );
}

#[test]
fn rsm_23_null_array_index_assignment_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("values[0] = 9;"),
        false,
        "cannot index a null array",
    );
}

#[test]
fn rsm_24_null_array_push_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("values.push(9);"),
        false,
        "cannot push to a null array",
    );
}

#[test]
fn rsm_25_null_array_pop_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values.pop());"),
        false,
        "cannot pop from a null array",
    );
}

#[test]
fn rsm_26_null_array_freeze_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values.freeze().len());"),
        false,
        "cannot freeze a null array",
    );
}

#[test]
fn rsm_27_null_array_to_string_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values.toString());"),
        false,
        "cannot convert a null array to String",
    );
}

#[test]
fn rsm_28_null_array_element_reference_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("write(&values[0]);").replacen(
            "fn main()",
            "fn write(value: &mut i64) { value = 9; }\nfn main()",
            1,
        ),
        false,
        "cannot index a null array",
    );
}

#[test]
fn rsm_29_null_reference_array_pop_fails() {
    assert_unhandled_fault(
        r#"
import std::collections::Array;
fn main() {
    let channel = Channel<Array<String>>::new();
    channel.close();
    let values = channel.recv();
    println(values.pop());
}
"#,
        false,
        "cannot pop from a null array",
    );
}

#[test]
fn rsm_30_release_null_array_len_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values.len());"),
        true,
        "cannot take the length of a null array",
    );
}

#[test]
fn rsm_31_null_array_fault_matches_lir_and_ast_backends() {
    let source = null_i64_array_program("println(values[0]);");
    let (lir, lir_ok) = compile_with_env_and_run_combined(&source, &[("WILLOW_LIR_BACKEND", "1")]);
    let (ast, ast_ok) = compile_with_env_and_run_combined(&source, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(!lir_ok, "LIR path unexpectedly succeeded: {lir}");
    assert!(!ast_ok, "AST path unexpectedly succeeded: {ast}");
    for (backend, out) in [("LIR", lir), ("AST", ast)] {
        assert!(
            out.contains("cannot index a null array"),
            "{backend} produced the wrong failure: {out}"
        );
    }
}

#[test]
fn rsm_32_direct_full_bounded_send_fails_without_recovery() {
    assert_unhandled_fault(
        r#"
fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    channel.send(2);
}
"#,
        false,
        "send on full bounded channel would block",
    );
}

#[test]
fn rsm_33_async_match_payload_can_be_captured_by_defer() {
    assert_async_output(
        r#"
async fn show(value: Option<String>) {
    match value {
        Some(text) => {
            defer println(text);
            let noise = "g" + "c";
            println(noise);
        },
        None => { println("none"); }
    }
    await sleep(0);
}
async fn main() { await show(Some("pay" + "load")); }
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
        "gc\npayload\n",
    );
}

#[test]
fn rsm_34_async_match_object_receiver_can_be_captured_by_defer() {
    assert_async_output(
        r#"
class Label {
    pub text: String;
    pub fn show(self) { println(self.text); }
}
async fn show(value: Option<Label>) {
    match value {
        Some(label) => { defer label.show(); println("body"); },
        None => { println("none"); }
    }
    await sleep(0);
}
async fn main() { await show(Some(new Label("object" + "-root"))); }
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
        "body\nobject-root\n",
    );
}

#[test]
fn rsm_35_multiple_gc_operands_survive_nested_cancel_cleanup() {
    assert_async_output(
        r#"
fn cleanup(first: String, second: String) { println(first + second); }
async fn waiting() {
    if true {
        let first = "left" + "-";
        let second = "right" + "!";
        defer cleanup(first, second);
        await sleep(10000);
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "left-right!\ntrue\n",
    );
}

#[test]
fn rsm_36_async_lexical_defer_works_in_release_mode() {
    let (out, ok) = compile_and_run_release(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        defer println(i);
        await sleep(0);
        i = i + 1;
    }
}
"#,
    );
    assert!(ok, "release async defer failed: {out}");
    assert_eq!(out, "0\n1\n2\n");
}

#[test]
fn rsm_37_eight_workers_run_each_task_defer_once() {
    let source = r#"
async fn worker(tag: i64) -> i64 {
    defer println(tag);
    await sleep(1);
    return tag;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    let c = worker(3);
    let d = worker(4);
    let e = worker(5);
    let f = worker(6);
    let g = worker(7);
    let h = worker(8);
    println(await a + await b + await c + await d + await e + await f + await g + await h);
}
"#;
    let (out, ok, timed_out) =
        compile_and_run_with_env_timeout(source, &[("WILLOW_WORKERS", "8")], ASYNC_TIMEOUT);
    assert!(!timed_out, "worker fixture timed out: {out}");
    assert!(ok, "worker fixture failed: {out}");
    let mut lines = out.lines().collect::<Vec<_>>();
    let total = lines.pop().expect("sum output");
    assert_eq!(total, "36");
    lines.sort_unstable();
    assert_eq!(lines, ["1", "2", "3", "4", "5", "6", "7", "8"]);
}

#[test]
fn rsm_38_gc_all_and_one_poll_budget_keep_defer_operands_alive() {
    assert_async_output(
        r#"
async fn main() {
    let mut i = 0;
    while i < 5 {
        let value = "v" + i.toString();
        defer println(value);
        await sleep(0);
        i = i + 1;
    }
}
"#,
        &[("WILLOW_GC_STRESS", "all"), ("WILLOW_TASK_BUDGET", "1")],
        "v0\nv1\nv2\nv3\nv4\n",
    );
}

#[test]
fn rsm_39_tiny_quantum_preserves_while_defer_scope() {
    assert_async_output(
        r#"
async fn main() {
    let mut total = 0;
    let mut i = 0;
    while i < 2000 {
        if i % 500 == 0 { defer println(i); }
        total = total + i;
        i = i + 1;
    }
    println(total);
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_TIME_QUANTUM_MS", "1")],
        "0\n500\n1000\n1500\n1999000\n",
    );
}

#[test]
fn rsm_40_selected_arm_gc_binding_survives_workers_and_preemption() {
    assert_async_output(
        r#"
async fn feed(channel: Channel<String>) {
    await sleep(2);
    channel.send("multi" + "worker");
}
async fn choose(channel: Channel<String>) {
    select {
        let value = channel.recv() => {
            defer println(value);
            let noise = "pre" + "empt";
            println(noise);
        }
        sleep(5000) => { println("timeout"); }
    }
}
async fn main() {
    let channel = Channel<String>::new();
    let producer = feed(channel);
    let consumer = choose(channel);
    await consumer;
    await producer;
}
"#,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TIME_QUANTUM_MS", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
        "preempt\nmultiworker\n",
    );
}

#[test]
fn rsm_41_nil_field_assignment_fails() {
    let out = assert_unhandled_fault(
        r#"
class AssignNode { pub value: i64; }
fn main() {
    let channel = Channel<AssignNode>::new();
    channel.close();
    let node = channel.recv();
    node.value = 3;
}
"#,
        false,
        "nil dereference",
    );
    assert!(out.contains("`value`"), "missing assignment context: {out}");
}

#[test]
fn rsm_42_release_nil_returned_from_helper_fails() {
    assert_unhandled_fault(
        r#"
class ReturnNode { pub value: i64; }
fn get_nil() -> ReturnNode {
    let channel = Channel<ReturnNode>::new();
    channel.close();
    return channel.recv();
}
fn main() { println(get_nil().value); }
"#,
        true,
        "nil dereference",
    );
}

#[test]
fn rsm_43_async_nil_access_after_suspension_fails() {
    assert_unhandled_fault(
        r#"
class AsyncNilNode { pub value: i64; }
async fn main() {
    await sleep(1);
    let channel = Channel<AsyncNilNode>::new();
    channel.close();
    let node = channel.recv();
    println(node.value);
}
"#,
        false,
        "nil dereference",
    );
}

#[test]
fn rsm_44_nil_nested_field_fails_at_the_inner_receiver() {
    let out = assert_unhandled_fault(
        r#"
class ChildNode { pub value: i64; }
class ParentNode { pub child: ChildNode; }
fn main() {
    let channel = Channel<ChildNode>::new();
    channel.close();
    let child = channel.recv();
    let parent = new ParentNode(child);
    println(parent.child.value);
}
"#,
        false,
        "nil dereference",
    );
    assert!(out.contains("`value`"), "wrong failing hop: {out}");
}

#[test]
fn rsm_45_release_nullable_method_after_narrowing_does_not_false_positive() {
    let (out, ok) = compile_and_run_release(
        r#"
class NarrowNode {
    pub value: i64;
    pub fn read(self) -> i64 { return self.value; }
}
fn read(node: NarrowNode?) -> i64 {
    if node == nil { return -1; }
    return node.read();
}
fn main() {
    println(read(new NarrowNode(7)));
    println(read(nil));
}
"#,
    );
    assert!(ok, "release narrowing failed: {out}");
    assert_eq!(out, "7\n-1\n");
}

#[test]
fn rsm_46_nil_field_fault_matches_lir_and_ast_backends() {
    let source = r#"
class BackendNilNode { pub value: i64; }
fn read(node: BackendNilNode) -> i64 { return node.value; }
fn main() {
    let channel = Channel<BackendNilNode>::new();
    channel.close();
    println(read(channel.recv()));
}
"#;
    let (lir, lir_ok) = compile_with_env_and_run_combined(source, &[("WILLOW_LIR_BACKEND", "1")]);
    let (ast, ast_ok) = compile_with_env_and_run_combined(source, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(!lir_ok, "LIR nil access unexpectedly succeeded: {lir}");
    assert!(!ast_ok, "AST nil access unexpectedly succeeded: {ast}");
    assert!(lir.contains("nil dereference"), "wrong LIR failure: {lir}");
    assert!(ast.contains("nil dereference"), "wrong AST failure: {ast}");
}

#[test]
fn rsm_47_debug_nil_interface_dispatch_keeps_method_context() {
    let out = assert_unhandled_fault(
        r#"
interface DebugReader { fn read(self) -> i64; }
class DebugReaderImpl implements DebugReader {
    pub fn read(self) -> i64 { return 1; }
}
fn main() {
    let channel = Channel<DebugReaderImpl>::new();
    channel.close();
    let concrete = channel.recv();
    let reader: DebugReader = concrete;
    println(reader.read());
}
"#,
        false,
        "nil dereference",
    );
    assert!(out.contains("`read`"), "missing method context: {out}");
}

#[test]
fn rsm_48_nil_check_precedes_field_load_under_gc_stress() {
    assert_unhandled_fault(
        r#"
class StressNilNode { pub text: String; }
fn main() {
    let noise = "before" + "-fault";
    println(noise);
    let channel = Channel<StressNilNode>::new();
    channel.close();
    let node = channel.recv();
    println(node.text);
}
"#,
        false,
        "nil dereference",
    );
}

#[test]
fn rsm_49_select_arm_multiple_defers_are_lifo() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    select {
        let value = channel.recv() => {
            defer println("first");
            defer println("second");
            println(value);
        }
    }
}
"#,
        &[],
        "1\nsecond\nfirst\n",
    );
}

#[test]
fn rsm_50_return_from_select_arm_unwinds_arm_and_function_scopes() {
    assert_async_output(
        r#"
async fn choose() -> i64 {
    defer println("function");
    let channel = Channel<i64>::with_capacity(1);
    channel.send(5);
    select {
        let value = channel.recv() => {
            defer println("arm");
            return value;
        }
    }
    return 0;
}
async fn main() { println(await choose()); }
"#,
        &[],
        "arm\nfunction\n5\n",
    );
}

#[test]
fn rsm_51_try_from_select_arm_unwinds_arm_and_function_scopes() {
    assert_async_output(
        r#"
fn fail() -> Result<i64, String> { return Err("select-error"); }
async fn choose() -> Result<i64, String> {
    defer println("function");
    let channel = Channel<i64>::new();
    select {
        let value = channel.recv() => { return Ok(value); }
        default => {
            defer println("arm");
            let value = fail()?;
            return Ok(value);
        }
    }
    return Ok(0);
}
async fn main() {
    match await choose() {
        Ok(value) => println(value),
        Err(error) => println(error)
    }
}
"#,
        &[],
        "arm\nfunction\nselect-error\n",
    );
}

#[test]
fn rsm_52_continue_from_select_arm_flushes_before_next_iteration() {
    assert_async_output(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        let channel = Channel<i64>::with_capacity(1);
        channel.send(i);
        i = i + 1;
        select {
            let value = channel.recv() => {
                defer println(value);
                continue;
            }
        }
    }
    println("done");
}
"#,
        &[],
        "0\n1\n2\ndone\n",
    );
}

#[test]
fn rsm_53_break_from_select_arm_flushes_before_loop_exit() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(4);
    while true {
        defer println("loop");
        select {
            let value = channel.recv() => {
                defer println(value);
                break;
            }
        }
    }
    println("done");
}
"#,
        &[],
        "4\nloop\ndone\n",
    );
}

#[test]
fn rsm_54_nested_select_arms_unwind_inner_before_outer() {
    assert_async_output(
        r#"
async fn main() {
    let first = Channel<i64>::with_capacity(1);
    let second = Channel<i64>::with_capacity(1);
    first.send(1);
    second.send(2);
    select {
        let one = first.recv() => {
            defer println(one);
            select {
                let two = second.recv() => {
                    defer println(two);
                    println("body");
                }
            }
        }
    }
}
"#,
        &[],
        "body\n2\n1\n",
    );
}

#[test]
fn rsm_55_repeated_select_site_consumes_each_defer_instance() {
    assert_async_output(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        let channel = Channel<i64>::with_capacity(1);
        channel.send(i);
        select {
            let value = channel.recv() => {
                defer println(value);
                println("body");
            }
        }
        i = i + 1;
    }
}
"#,
        &[],
        "body\n0\nbody\n1\nbody\n2\n",
    );
}

#[test]
fn rsm_56_cancel_inside_timeout_arm_runs_pending_defer() {
    assert_async_output(
        r#"
async fn waiting() {
    let channel = Channel<i64>::new();
    select {
        let value = channel.recv() => { println(value); }
        sleep(1) => {
            defer println("timeout-cleanup");
            await sleep(10000);
        }
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "timeout-cleanup\ntrue\n",
    );
}

#[test]
fn rsm_57_cancel_inside_default_arm_runs_pending_defer() {
    assert_async_output(
        r#"
async fn waiting() {
    let channel = Channel<i64>::new();
    select {
        let value = channel.recv() => { println(value); }
        default => {
            defer println("default-cleanup");
            await sleep(10000);
        }
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "default-cleanup\ntrue\n",
    );
}

#[test]
fn rsm_58_non_selected_gc_arm_does_not_keep_stale_defer() {
    assert_async_output(
        r#"
async fn main() {
    let ready = Channel<String>::with_capacity(1);
    let blocked = Channel<String>::new();
    ready.send("chosen" + "-value");
    select {
        let value = ready.recv() => { defer println(value); }
        let value = blocked.recv() => { defer println(value); }
    }
    let noise = "after" + "-gc";
    println(noise);
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
        "chosen-value\nafter-gc\n",
    );
}

#[test]
fn rsm_59_select_send_arm_defer_keeps_string_operand_alive() {
    assert_async_output(
        r#"
async fn main() {
    let channel = Channel<String>::with_capacity(1);
    let payload = "send" + "-value";
    select {
        channel.send(payload) => {
            defer println(payload);
            let noise = "g" + "c";
            println(noise);
        }
    }
    println(channel.recv());
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
        "gc\nsend-value\nsend-value\n",
    );
}

#[test]
fn rsm_60_select_task_string_binding_survives_gc_stress() {
    assert_async_output(
        r#"
async fn value() -> String { await sleep(1); return "task" + "-value"; }
async fn main() {
    let task = value();
    select {
        let result = await task => {
            defer println(result);
            let noise = "g" + "c";
            println(noise);
        }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
        "gc\ntask-value\n",
    );
}

#[test]
fn rsm_61_zero_iteration_while_registers_no_defer() {
    assert_async_output(
        r#"
async fn main() {
    let mut active = false;
    while active {
        defer println("wrong");
        active = false;
    }
    println("done");
}
"#,
        &[],
        "done\n",
    );
}

#[test]
fn rsm_62_return_from_while_unwinds_iteration_and_function() {
    assert_async_output(
        r#"
async fn value() -> i64 {
    defer println("function");
    while true {
        defer println("iteration");
        return 12;
    }
    return 0;
}
async fn main() { println(await value()); }
"#,
        &[],
        "iteration\nfunction\n12\n",
    );
}

#[test]
fn rsm_63_try_from_while_unwinds_iteration_and_function() {
    assert_async_output(
        r#"
fn fail() -> Result<i64, String> { return Err("while-error"); }
async fn value() -> Result<i64, String> {
    defer println("function");
    let mut active = true;
    while active {
        defer println("iteration");
        let number = fail()?;
        active = false;
        return Ok(number);
    }
    return Ok(0);
}
async fn main() {
    match await value() {
        Ok(number) => println(number),
        Err(error) => println(error)
    }
}
"#,
        &[],
        "iteration\nfunction\nwhile-error\n",
    );
}

#[test]
fn rsm_64_while_iteration_multiple_defers_are_lifo() {
    assert_async_output(
        r#"
async fn main() {
    let mut i = 0;
    while i < 2 {
        defer println("first");
        defer println("second");
        println(i);
        i = i + 1;
    }
}
"#,
        &[],
        "0\nsecond\nfirst\n1\nsecond\nfirst\n",
    );
}

#[test]
fn rsm_65_inner_while_break_does_not_flush_outer_early() {
    assert_async_output(
        r#"
async fn main() {
    let mut outer = 0;
    while outer < 1 {
        defer println("outer");
        while true {
            defer println("inner");
            break;
        }
        println("after-inner");
        outer = outer + 1;
    }
}
"#,
        &[],
        "inner\nafter-inner\nouter\n",
    );
}

#[test]
fn rsm_66_cancel_second_while_iteration_does_not_rerun_first() {
    assert_async_output(
        r#"
async fn waiting() {
    let mut i = 0;
    while i < 3 {
        defer println(i);
        if i == 1 { await sleep(10000); }
        i = i + 1;
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "0\n1\ntrue\n",
    );
}

#[test]
fn rsm_67_cancel_nested_while_unwinds_inner_then_outer() {
    assert_async_output(
        r#"
async fn waiting() {
    let mut outer = 0;
    while outer < 1 {
        defer println("outer");
        while true {
            defer println("inner");
            await sleep(10000);
        }
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4")],
        "inner\nouter\ntrue\n",
    );
}

#[test]
fn rsm_68_while_class_receiver_survives_gc_stress_cancel() {
    assert_async_output(
        r#"
class CleanupLabel {
    pub text: String;
    pub fn show(self) { println(self.text); }
}
async fn waiting() {
    let mut active = true;
    while active {
        let label = new CleanupLabel("class" + "-cleanup");
        defer label.show();
        await sleep(10000);
        active = false;
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "class-cleanup\ntrue\n",
    );
}

#[test]
fn rsm_69_while_array_operand_survives_gc_stress() {
    assert_async_output(
        r#"
import std::collections::Array;
fn show(values: Array<String>) { println(values.toString()); }
async fn main() {
    let mut i = 0;
    while i < 2 {
        let values: Array<String> = ["v" + i.toString()];
        defer show(values);
        let noise = "g" + "c";
        println(noise);
        await sleep(0);
        i = i + 1;
    }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
        "gc\n[\"v0\"]\ngc\n[\"v1\"]\n",
    );
}

#[test]
fn rsm_70_release_while_break_continue_scope_is_correct() {
    let (out, ok) = compile_and_run_release(
        r#"
async fn main() {
    let mut i = 0;
    while i < 4 {
        let current = i;
        i = i + 1;
        defer println(current);
        if current == 1 { continue; }
        if current == 2 { break; }
        println("body");
    }
    println("done");
}
"#,
    );
    assert!(ok, "release loop defer failed: {out}");
    assert_eq!(out, "body\n0\n1\n2\ndone\n");
}

#[test]
fn rsm_71_release_null_array_index_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values[0]);"),
        true,
        "cannot index a null array",
    );
}

#[test]
fn rsm_72_release_null_array_to_string_fails() {
    assert_unhandled_fault(
        &null_i64_array_program("println(values.toString());"),
        true,
        "cannot convert a null array to String",
    );
}

#[test]
fn rsm_73_async_null_array_after_await_fails() {
    assert_unhandled_fault(
        r#"
import std::collections::Array;
async fn main() {
    await sleep(1);
    let channel = Channel<Array<i64>>::new();
    channel.close();
    let values = channel.recv();
    println(values.len());
}
"#,
        false,
        "cannot take the length of a null array",
    );
}

#[test]
fn rsm_74_null_array_passed_to_helper_fails_at_use() {
    assert_unhandled_fault(
        r#"
import std::collections::Array;
fn read(values: Array<i64>) -> i64 { return values[0]; }
fn main() {
    let channel = Channel<Array<i64>>::new();
    channel.close();
    println(read(channel.recv()));
}
"#,
        false,
        "cannot index a null array",
    );
}

#[test]
fn rsm_75_null_array_check_precedes_negative_index_check() {
    let out = assert_unhandled_fault(
        &null_i64_array_program("println(values[-1]);"),
        false,
        "cannot index a null array",
    );
    assert!(
        !out.contains("length is"),
        "null receiver must be diagnosed before bounds: {out}"
    );
}

#[test]
fn rsm_76_null_reference_array_to_string_fails() {
    assert_unhandled_fault(
        r#"
import std::collections::Array;
fn main() {
    let channel = Channel<Array<String>>::new();
    channel.close();
    println(channel.recv().toString());
}
"#,
        false,
        "cannot convert a null array to String",
    );
}

#[test]
fn rsm_77_release_empty_open_channel_recv_fails() {
    assert_unhandled_fault(
        r#"
fn main() {
    let channel = Channel<i64>::new();
    println(channel.recv());
}
"#,
        true,
        "recv on empty open channel would block",
    );
}

#[test]
fn rsm_78_release_invalid_channel_capacity_fails() {
    assert_unhandled_fault(
        "fn main() { let channel = Channel<i64>::with_capacity(0); channel.send(1); }",
        true,
        "channel capacity must be positive",
    );
}

#[test]
fn rsm_79_release_full_bounded_channel_send_fails() {
    assert_unhandled_fault(
        r#"
fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    channel.send(2);
}
"#,
        true,
        "send on full bounded channel would block",
    );
}

#[test]
fn rsm_80_release_bounded_channel_success_has_no_false_fault() {
    let (out, ok) = compile_and_run_release(
        r#"
fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(7);
    println(channel.recv());
    channel.send(8);
    println(channel.recv());
}
"#,
    );
    assert!(ok, "release bounded channel failed: {out}");
    assert_eq!(out, "7\n8\n");
}

#[test]
fn rsm_81_release_send_on_closed_channel_remains_noop() {
    let (out, ok) = compile_and_run_release(
        r#"
fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.close();
    channel.send(7);
    println(channel.recv());
}
"#,
    );
    assert!(ok, "closed send changed semantics: {out}");
    assert_eq!(out, "0\n");
}

#[test]
fn rsm_82_full_reference_channel_send_fails_without_corrupting_diagnostic() {
    assert_unhandled_fault(
        r#"
fn main() {
    let channel = Channel<String>::with_capacity(1);
    channel.send("first" + "-value");
    channel.send("second" + "-value");
}
"#,
        false,
        "send on full bounded channel would block",
    );
}

#[test]
fn rsm_83_async_bounded_send_parks_instead_of_raising() {
    assert_async_output(
        r#"
async fn producer(channel: Channel<i64>) {
    channel.send(1);
    channel.send(2);
}
async fn main() {
    let channel = Channel<i64>::with_capacity(1);
    let task = producer(channel);
    await sleep(10);
    println(channel.recv());
    println(channel.recv());
    await task;
}
"#,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "scheduler")],
        "1\n2\n",
    );
}

#[test]
fn rsm_84_full_send_fault_matches_lir_and_ast_backends() {
    let source = r#"
fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    channel.send(2);
}
"#;
    let (lir, lir_ok) = compile_with_env_and_run_combined(source, &[("WILLOW_LIR_BACKEND", "1")]);
    let (ast, ast_ok) = compile_with_env_and_run_combined(source, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(!lir_ok, "LIR full send unexpectedly succeeded: {lir}");
    assert!(!ast_ok, "AST full send unexpectedly succeeded: {ast}");
    assert!(
        lir.contains("send on full bounded channel would block"),
        "{lir}"
    );
    assert!(
        ast.contains("send on full bounded channel would block"),
        "{ast}"
    );
}

#[test]
fn rsm_85_null_bool_array_to_string_fails() {
    assert_unhandled_fault(
        r#"
import std::collections::Array;
fn main() {
    let channel = Channel<Array<bool>>::new();
    channel.close();
    println(channel.recv().toString());
}
"#,
        false,
        "cannot convert a null array to String",
    );
}

#[test]
fn rsm_86_select_defer_works_in_release_mode() {
    let (out, ok) = compile_and_run_release(
        r#"
async fn main() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(6);
    select {
        let value = channel.recv() => { defer println("cleanup"); println(value); }
    }
}
"#,
    );
    assert!(ok, "release select defer failed: {out}");
    assert_eq!(out, "6\ncleanup\n");
}

#[test]
fn rsm_87_select_cancel_cleanup_survives_workers_and_scheduler_stress() {
    assert_async_output(
        r#"
async fn waiting() {
    let channel = Channel<i64>::with_capacity(1);
    channel.send(1);
    select {
        let value = channel.recv() => {
            defer println(value);
            await sleep(10000);
        }
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "scheduler")],
        "1\ntrue\n",
    );
}

#[test]
fn rsm_88_while_cancel_cleanup_survives_full_gc_stress() {
    assert_async_output(
        r#"
async fn waiting() {
    let mut active = true;
    while active {
        let value = "while" + "-gc";
        defer println(value);
        await sleep(10000);
        active = false;
    }
}
async fn main() {
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(task.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "all")],
        "while-gc\ntrue\n",
    );
}

#[test]
fn rsm_89_async_match_defer_works_in_release_mode() {
    let (out, ok) = compile_and_run_release(
        r#"
async fn show(value: Option<String>) {
    match value {
        Some(text) => { defer println(text); println("body"); },
        None => { println("none"); }
    }
    await sleep(0);
}
async fn main() { await show(Some("release-match")); }
"#,
    );
    assert!(ok, "release match defer failed: {out}");
    assert_eq!(out, "body\nrelease-match\n");
}

#[test]
fn rsm_90_object_receiver_defers_survive_multiworker_gc_stress() {
    let source = r#"
class WorkerLabel {
    pub value: i64;
    pub fn show(self) { println(self.value); }
}
async fn worker(tag: i64) -> i64 {
    let label = new WorkerLabel(tag);
    defer label.show();
    await sleep(1);
    return tag;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    let c = worker(3);
    let d = worker(4);
    let e = worker(5);
    let f = worker(6);
    let g = worker(7);
    let h = worker(8);
    println(await a + await b + await c + await d + await e + await f + await g + await h);
}
"#;
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        source,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "alloc")],
        ASYNC_TIMEOUT,
    );
    assert!(!timed_out, "object receiver fixture timed out: {out}");
    assert!(ok, "object receiver fixture failed: {out}");
    let mut lines = out.lines().collect::<Vec<_>>();
    let total = lines.pop().expect("sum output");
    assert_eq!(total, "36");
    lines.sort_unstable();
    assert_eq!(lines, ["1", "2", "3", "4", "5", "6", "7", "8"]);
}
