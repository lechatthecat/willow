use super::*;

#[test]
fn async_retention_dead_locals_run() {
    let (output, ok) = compile_and_run(include_str!("../../../example/async_dead_locals.wi"));
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "42\n42\n2\n3");
}

#[test]
fn async_retention_live_interface_array_rejected() {
    let source = r#"
import std::collections::Array;
interface Value { fn get(self) -> i64; }
class Number implements Value {
    pub value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
async fn main() {
    let values: Array<Value> = [new Number(42)];
    await sleep(0);
    println(values[0].get());
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(!ok, "live non-Send interface array must be rejected");
    assert!(errors.contains("E2402"), "{errors}");
    assert!(errors.contains("`values` is retained"), "{errors}");
    assert!(errors.contains("can suspend here"), "{errors}");
}

#[test]
fn async_retention_no_await_forced_preemption() {
    let (output, ok) = compile_and_run_with_runtime_env(
        include_str!("../../../example/async_dead_locals.wi"),
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "42\n42\n2\n3");
}

#[test]
fn async_retention_intermediate_rejections() {
    for operand in [
        "identity(op)",
        "identity(identity(op))",
        "holder.value",
        "ops[0]",
    ] {
        let source = format!(
            r#"
import std::collections::Array;
class Holder {{ pub value: closure(i64) -> i64; }}
fn identity(op: closure(i64) -> i64) -> closure(i64) -> i64 {{ return op; }}
fn consume(op: closure(i64) -> i64, n: i64) {{ println(op(n)); }}
async fn number() -> i64 {{ return 1; }}
async fn main() {{
    let op: closure(i64) -> i64 = |x| x;
    let holder = new Holder(op);
    let ops: Array<closure(i64) -> i64> = [op];
    consume({operand}, await number());
}}
"#
        );
        let (ok, errors) = compile_with_data_race_check(&source);
        assert!(!ok, "{operand}");
        assert!(
            errors.contains("E2402")
                && errors.contains("intermediate value")
                && errors.contains("operand evaluation can suspend"),
            "{operand}: {errors}"
        );
    }
}

#[test]
fn async_retention_patterns_and_saved_callee_rejected() {
    for (body, label) in [
        (
            "match op { f => { await sleep(0); println(f(1)); } }",
            "`f` is retained",
        ),
        (
            "match op { f if await choose() => {}, g => { println(g(1)); } }",
            "intermediate value",
        ),
        ("defer op(1); await sleep(0);", "deferred callee"),
        (
            "defer op(1); op = |x| x + 2; await sleep(0);",
            "deferred callee",
        ),
    ] {
        let source = format!(
            "async fn choose() -> bool {{ return false; }} async fn main() {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
        );
        let (ok, errors) = compile_with_data_race_check(&source);
        assert!(
            !ok && errors.contains("E2402") && errors.contains(label),
            "{body}: {errors}"
        );
    }
}

#[test]
fn async_retention_ordered_cleanup_run_and_reject() {
    let (output, ok) = compile_and_run_with_runtime_env(
        r#"
async fn work(flag: bool) {
    let mut op: closure(i64) -> i64 = |x| x + 90;
    defer { if flag { op = |x| x + 1; } else { op = |x| x + 2; } println(op(1)); }
    await sleep(0);
}
async fn main() { await work(true); await work(false); }
"#,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "2\n3");
    let (ok, errors) = compile_with_data_race_check(
        r#"
async fn work(flag: bool) {
    let mut op: closure(i64) -> i64 = |x| x;
    defer { if flag { op = |x| x + 1; } println(op(1)); }
    await sleep(0);
}
async fn main() { await work(false); }
"#,
    );
    assert!(
        !ok && errors.contains("E2402") && errors.contains("`op` is retained"),
        "{errors}"
    );
}

#[test]
fn async_retention_cleanup_continuations_run() {
    let source = r#"
async fn main() {
    let mut op: closure(i64) -> i64 = |x| x;
    await sleep(0);
    if true { defer { op = |x| x + 1; } }
    println(op(1));
    while true { await sleep(0); defer { op = |x| x + 1; } break; }
    println(op(1));
    op = |x| x;
    let mut i = 0;
    while i < 2 {
        println(op(1));
        await sleep(0);
        defer { op = |x| x + 1; }
        i = i + 1;
        continue;
    }
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "2\n2\n1\n2");
}

#[test]
fn async_retention_exhaustive_match_overwrites_run() {
    let source = r#"
enum Tag { A, B }
async fn work(flag: bool, tag: Tag) {
    let mut op: closure(i64) -> i64 = |x| x;
    await sleep(0);
    match flag { true => { op = |x| x + 1; }, false => { op = |x| x + 2; } }
    println(op(1));
    await sleep(0);
    match tag { Tag::A => { op = |x| x + 3; }, Tag::B => { op = |x| x + 4; } }
    println(op(1));
    defer {
        match flag { true => { op = |x| x + 5; }, false => { op = |x| x + 6; } }
        println(op(1));
        match tag { Tag::A => { op = |x| x + 7; }, Tag::B => { op = |x| x + 8; } }
        println(op(1));
    }
    await sleep(0);
}
async fn main() { await work(true, Tag::A); await work(false, Tag::B); }
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "2\n4\n6\n8\n3\n5\n7\n9");
}

#[test]
fn async_retention_literal_expression_branches_run() {
    let source = r#"
async fn choose() -> bool { println(99); return true; }
async fn main() {
    let op: closure(i64) -> i64 = |x| x;
    let a = false && await choose();
    let b = true || await choose();
    let c = true ? false : await choose();
    let d = false ? await choose() : true;
    println(op(1));
    println(a); println(b); println(c); println(d);
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "1\nfalse\ntrue\nfalse\ntrue");
    for expr in [
        "true && await choose()",
        "false || await choose()",
        "false ? false : await choose()",
    ] {
        let source = format!(
            "async fn choose() -> bool {{ return true; }} async fn main() {{ let op: closure(i64) -> i64 = |x| x; let b = {expr}; println(op(1)); }}"
        );
        let (ok, errors) = compile_with_data_race_check(&source);
        assert!(!ok && errors.contains("E2402"), "{expr}: {errors}");
    }
}

#[test]
fn async_retention_bare_variant_rejections() {
    for (a, b) in [("A", "B"), ("Tag::A", "Tag::B")] {
        for cleanup in [false, true] {
            let body = if cleanup {
                format!(
                    "defer {{ match tag {{ {a} => {{}}, {b} => {{ println(op(1)); }} }} }} await sleep(0);"
                )
            } else {
                format!("match tag {{ {a} => {{}}, {b} => {{ await sleep(0); println(op(1)); }} }}")
            };
            let source = format!(
                "enum Tag {{ A, B }} async fn work(tag: Tag) {{ let op: closure(i64) -> i64 = |x| x; {body} }} async fn main() {{ await work(Tag::B); }}"
            );
            let (ok, errors) = compile_with_data_race_check(&source);
            assert!(
                !ok && errors.contains("E2402") && errors.contains("`op` is retained"),
                "{a} cleanup={cleanup}: {errors}"
            );
        }
    }
}

#[test]
fn async_retention_literal_match_guards_run() {
    let source = r#"
async fn main() {
    let mut op: closure(i64) -> i64 = |x| x;
    match true { _ if false => { await sleep(0); println(op(99)); }, _ => {} }
    match true { _ if true => {}, _ => { await sleep(0); println(op(99)); } }
    println(op(1));
    await sleep(0);
    match true { _ if true => { op = |x| x + 1; }, _ => { println(op(99)); } }
    println(op(1));
    defer {
        match true { _ if false => { println(op(99)); }, _ => { op = |x| x + 2; } }
        println(op(1));
        match true { _ if true => { op = |x| x + 3; }, _ => { println(op(99)); } }
        println(op(1));
    }
    await sleep(0);
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "1\n2\n3\n4");
}

#[test]
fn async_retention_never_cli_diagnostics() {
    for (body, bad) in [
        (r#"panic("stop"); await sleep(0); println(op(1));"#, false),
        (r#"await sleep(0); panic("stop"); println(op(1));"#, false),
        (r#"await sleep(0); println(op(1)); panic("stop");"#, true),
        (
            r#"defer { panic("stop"); println(op(1)); } await sleep(0);"#,
            false,
        ),
        (
            r#"defer { println(op(1)); } defer panic("stop"); await sleep(0);"#,
            true,
        ),
    ] {
        let source = format!("async fn main() {{ let op: closure(i64) -> i64 = |x| x; {body} }}");
        let (ok, errors) = compile_with_data_race_check(&source);
        assert_eq!(ok, !bad, "{body}: {errors}");
        assert_eq!(errors.contains("E2402"), bad, "{body}: {errors}");
    }
}

#[test]
fn async_retention_never_cleanup_recovery_run() {
    let source = r#"
async fn unreachable() {
    let op: closure(i64) -> i64 = |x| x;
    if true {
        defer { recover(); }
        panic("recovered");
        await sleep(0);
        println(op(99));
    }
}
async fn main() {
    await unreachable();
    let mut op: closure(i64) -> i64 = |x| x;
    await sleep(0);
    if true {
        defer { recover(); }
        defer { op = |x| x + 1; panic("cleanup"); println(op(99)); }
    }
    println(op(1));
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "2");
}

#[test]
fn async_retention_exceptional_cleanup_cli() {
    for operation in [
        "fail();",
        "println(values[zero]);",
        "println(1 / zero);",
        "values[zero] = 2;",
    ] {
        for overwrite_first in [false, true] {
            let cleanup = if overwrite_first {
                format!("op = |x| x + 1; {operation}")
            } else {
                format!("{operation} op = |x| x + 1;")
            };
            let source = format!(
                "fn fail() {{ panic(\"stop\"); }} async fn main() {{ let zero = 0; let mut values = [1]; let mut op: closure(i64) -> i64 = |x| x; defer {{ println(op(1)); }} defer {{ {cleanup} }} await sleep(0); }}"
            );
            let (ok, errors) = compile_with_data_race_check(&source);
            assert_eq!(ok, overwrite_first, "{operation}: {errors}");
            assert_eq!(
                errors.contains("E2402"),
                !overwrite_first,
                "{operation}: {errors}"
            );
        }
    }
}

#[test]
fn async_retention_exceptional_cleanup_overwrite_run() {
    let source = r#"
fn fail() { panic("cleanup"); }
async fn main() {
    let mut op: closure(i64) -> i64 = |x| x;
    defer { recover(); }
    defer { println(op(1)); }
    defer { op = |x| x + 1; fail(); }
    await sleep(0);
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "2");
}

#[test]
fn async_retention_cancelled_select_commit() {
    let source = r#"
async fn slow() -> i64 { await sleep(1000); return 1; }
async fn main() {
    let t = slow();
    t.cancel();
    // `is_cancelled` reports only the request; `select` sees `t` ready once a
    // worker has finalized it. Awaiting it first blocks until then.
    if true {
        defer { recover(); }
        await t;
    }
    let mut op: closure(i64) -> i64 = |x| x;
    await sleep(0);
    if true {
        defer { recover(); }
        // OVERWRITE
        select {
            let value = await t => { op = |x| x + value; }
            default => { op = |x| x + 2; }
        }
    }
    println(op(1));
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(!ok && errors.contains("E2402"), "{errors}");
    let positive = source.replace("// OVERWRITE", "op = |x| x + 1;");
    let (ok, errors) = compile_with_data_race_check(&positive);
    assert!(ok, "{errors}");
    let (output, ok) = compile_and_run_with_runtime_env(
        &positive,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(ok, "{output}");
    assert_eq!(output.trim(), "2");
}
