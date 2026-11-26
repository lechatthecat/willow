use super::*;
use std::time::Duration;

// Each matrix row checks the panic message, skipped success continuation,
// lexical recovery, preservation of the buffered value, and closed-empty None.
// All executions have a hang guard; synchronization uses channels, not sleeps.
fn recovered_send(async_prefix: &str, ty: &str, value: &str, select: bool, bounded: bool) {
    let constructor = if bounded { "with_capacity(1)" } else { "new()" };
    let operation = if select {
        format!(
            "select {{ ch.send({value}) => {{ println(\"bad arm\"); }} default => {{ println(\"bad default\"); }} }}"
        )
    } else {
        format!("ch.send({value});")
    };
    let source = format!(
        r#"
{async_prefix}fn main() {{
    let ch = Channel<{ty}>::{constructor};
    ch.send({value});
    ch.close();
    if true {{
        defer match recover() {{ Some(info) => println(info.message), None => println("missing panic") }}
        {operation}
        println("bad continuation");
    }}
    println(ch.recv_opt().is_some());
    println(ch.recv_opt().is_none());
    println("after recovery");
}}
"#
    );
    let (out, ok) = compile_and_run_with_runtime_env(
        &source,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_WORKERS", "1")],
        Duration::from_secs(60),
    );
    assert!(ok, "{source}\n{out}");
    assert_eq!(out, "send on closed channel\ntrue\ntrue\nafter recovery\n");
}

macro_rules! closed_send_case {
    ($name:ident, $prefix:literal, $ty:literal, $value:literal, $select:literal, $bounded:literal) => {
        #[test]
        fn $name() {
            recovered_send($prefix, $ty, $value, $select, $bounded);
        }
    };
}

closed_send_case!(closed_send_sync_i64, "", "i64", "42", false, false);
closed_send_case!(closed_send_sync_bool, "", "bool", "true", false, true);
closed_send_case!(closed_send_sync_f64, "", "f64", "2.5", false, false);
closed_send_case!(
    closed_send_sync_string,
    "",
    "String",
    "\"value\" + 42.to_string()",
    false,
    true
);
closed_send_case!(closed_send_async_i64, "async ", "i64", "42", false, true);
closed_send_case!(
    closed_send_async_bool,
    "async ",
    "bool",
    "false",
    false,
    false
);
closed_send_case!(closed_send_async_f64, "async ", "f64", "2.5", false, true);
closed_send_case!(
    closed_send_async_string,
    "async ",
    "String",
    "\"value\" + 42.to_string()",
    false,
    false
);
closed_send_case!(closed_send_sync_select_i64, "", "i64", "42", true, true);
closed_send_case!(
    closed_send_sync_select_bool,
    "",
    "bool",
    "true",
    true,
    false
);
closed_send_case!(closed_send_sync_select_f64, "", "f64", "2.5", true, true);
closed_send_case!(
    closed_send_sync_select_string,
    "",
    "String",
    "\"value\" + 42.to_string()",
    true,
    false
);
closed_send_case!(
    closed_send_async_select_i64,
    "async ",
    "i64",
    "42",
    true,
    false
);
closed_send_case!(
    closed_send_async_select_bool,
    "async ",
    "bool",
    "false",
    true,
    true
);
closed_send_case!(
    closed_send_async_select_f64,
    "async ",
    "f64",
    "2.5",
    true,
    false
);
closed_send_case!(
    closed_send_async_select_string,
    "async ",
    "String",
    "\"value\" + 42.to_string()",
    true,
    true
);
closed_send_case!(
    closed_send_sync_select_inline_option,
    "",
    "Option<String>",
    "Some(\"kept\")",
    true,
    true
);
closed_send_case!(
    closed_send_async_inline_option,
    "async ",
    "Option<String>",
    "Some(\"kept\")",
    false,
    true
);
closed_send_case!(
    closed_send_async_select_inline_option,
    "async ",
    "Option<String>",
    "Some(\"kept\")",
    true,
    true
);

closed_send_case!(
    closed_send_select_none_infers_payload,
    "",
    "Option<i64>",
    "None",
    true,
    true
);
closed_send_case!(
    closed_send_select_result_infers_payload,
    "async ",
    "Result<i64, String>",
    "Ok(7)",
    true,
    false
);

#[test]
fn closed_send_select_rejects_wrong_variant_payload() {
    assert_compile_error_contains(
        r#"fn main() { let ch = Channel<Option<i64>>::new(); select { ch.send(Some("wrong")) => {} default => {} } }"#,
        &["error[E0201]"],
    );
}

#[test]
fn closed_send_unrecovered_exits_without_success() {
    for prefix in ["", "async "] {
        for operation in [
            "ch.send(2);",
            "select { ch.send(2) => { println(\"bad arm\"); } }",
        ] {
            let source = format!(
                "{prefix}fn main() {{ let ch = Channel<i64>::new(); ch.close(); {operation} println(\"bad continuation\"); }}"
            );
            let (out, ok) = compile_and_run_with_runtime_env(&source, &[], Duration::from_secs(60));
            assert!(!ok, "{out}");
            assert!(out.contains("send on closed channel"), "{out}");
            assert!(!out.contains("bad "), "{out}");
        }
    }
}

#[test]
fn closed_send_example_recovers_and_drains() {
    let (out, ok) = compile_and_run_with_runtime_env(
        include_str!("../../../example/channel_closed_send.wi"),
        &[("WILLOW_GC_STRESS", "alloc")],
        Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "send on closed channel\nsend on closed channel\naccepted\ntrue\n"
    );
}

#[test]
fn closed_send_sync_helper_propagates_and_runs_defers_once() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn send(ch: Channel<i64>) {
    defer println("inner defer");
    ch.send(1);
    println("bad helper continuation");
}
async fn main() {
    let ch = Channel<i64>::new();
    ch.close();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        defer println("outer defer");
        send(ch);
        println("bad caller continuation");
    }
    println("done");
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
        Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "inner defer\nouter defer\nsend on closed channel\ndone\n"
    );
}

#[test]
fn closed_send_select_entry_closes_channel_and_evaluates_value_once() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn value(ch: Channel<i64>) -> i64 { println("value"); ch.close(); return 2; }
async fn main() {
    let ch = Channel<i64>::new();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        select {
            ch.send(value(ch)) => { println("bad arm"); }
            default => { println("bad default"); }
        }
    }
    println(ch.recv_opt().is_none());
}
"#,
        &[],
        Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "value\nsend on closed channel\ntrue\n");
}

#[test]
fn closed_send_open_full_select_uses_default_then_sends_after_drain() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
async fn main() {
    let ch = Channel<i64>::with_capacity(1);
    ch.send(1);
    select { ch.send(2) => { println("bad"); } default => { println("full"); } }
    println(ch.recv());
    select { ch.send(3) => { println("sent"); } default => { println("bad"); } }
    println(ch.recv());
}
"#,
        &[],
        Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "full\n1\nsent\n3\n");
}

#[test]
fn closed_send_many_producers_recover_on_close() {
    let source = r#"
async fn sender(ch: Channel<String>, started: Channel<i64>, done: Channel<i64>) {
    if true {
    defer match recover() { Some(info) => { if info.message != "send on closed channel" { println("wrong panic"); } }, None => println("missing panic") }
    started.send(1);
    ch.send("rejected" + 7.to_string());
    println("bad success");
    }
    done.send(1);
}
async fn main() {
    let ch = Channel<String>::with_capacity(1);
    ch.send("kept");
    let started = Channel<i64>::new();
    let done = Channel<i64>::new();
    let a = sender(ch, started, done);
    let b = sender(ch, started, done);
    let c = sender(ch, started, done);
    println(started.recv() + started.recv() + started.recv());
    ch.close();
    println(done.recv() + done.recv() + done.recv());
    await a; await b; await c;
    println(ch.recv());
    println(ch.recv_opt().is_none());
}
"#;
    for select in [false, true] {
        let source = if select {
            source.replace(
                "ch.send(\"rejected\" + 7.to_string());",
                "select { ch.send(\"rejected\" + 7.to_string()) => { println(\"bad arm\"); } }",
            )
        } else {
            source.to_owned()
        };
        for workers in ["1", "4"] {
            let (out, ok) = compile_and_run_with_runtime_env(
                &source,
                &[("WILLOW_WORKERS", workers), ("WILLOW_GC_STRESS", "alloc")],
                Duration::from_secs(60),
            );
            assert!(ok, "workers={workers}, select={select}: {out}");
            assert_eq!(out, "3\n3\nkept\ntrue\n");
        }
    }
}

#[test]
fn closed_send_repeated_boxed_select_recovery_keeps_live_values() {
    for prefix in ["", "async "] {
        let source = format!(
            r#"
{prefix}fn main() {{
    let ch = Channel<Option<String>>::new();
    ch.close();
    let keep = Channel<String>::new();
    let mut recovered = 0;
    for i in 0..64 {{
        if true {{
            defer match recover() {{ Some(_) => {{ recovered = recovered + 1; }}, None => {{}} }}
            select {{ ch.send(Some("rejected" + i.to_string())) => {{ println("bad"); }} }}
        }}
        keep.send("live" + i.to_string());
        println(keep.recv());
    }}
    println(recovered);
}}
"#
        );
        let (out, ok) = compile_and_run_with_runtime_env(
            &source,
            &[("WILLOW_GC_STRESS", "alloc")],
            Duration::from_secs(60),
        );
        assert!(ok, "{out}");
        let mut expected = (0..64).map(|i| format!("live{i}\n")).collect::<String>();
        expected.push_str("64\n");
        assert_eq!(out, expected);
    }
}

#[test]
fn closed_send_select_cleans_losing_waiters_before_recovery() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
async fn main() {
    let full = Channel<i64>::with_capacity(1);
    full.send(11);
    let empty = Channel<i64>::new();
    let closed = Channel<i64>::new();
    closed.close();
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        select {
            full.send(22) => { println("bad full"); }
            let v = empty.recv() => { println(v); }
            closed.send(33) => { println("bad closed"); }
        }
    }
    println(full.recv());
    full.send(44);
    empty.send(55);
    println(full.recv());
    println(empty.recv());
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
        Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "send on closed channel\n11\n44\n55\n");
}

#[test]
fn closed_send_sync_blocked_helper_is_woken_by_close() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
async fn closer(ch: Channel<String>, started: Channel<i64>) {
    started.recv();
    ch.close();
}
fn send(ch: Channel<String>, started: Channel<i64>) {
    started.send(1);
    ch.send("rejected" + 7.to_string());
    println("bad helper");
}
async fn main() {
    let ch = Channel<String>::with_capacity(1);
    ch.send("kept");
    let started = Channel<i64>::new();
    let task = closer(ch, started);
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        send(ch, started);
        println("bad caller");
    }
    await task;
    println(ch.recv());
    println(ch.recv_opt().is_none());
}
"#,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_GC_STRESS", "alloc")],
        Duration::from_secs(60),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "send on closed channel\nkept\ntrue\n");
}
