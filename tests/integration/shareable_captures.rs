use super::support::*;

#[test]
fn shareable_captures_execute_across_tasks_channels_fields_and_returns() {
    let source = include_str!("../../example/shareable_captures.wi");
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "20\n20\n14\n17\n");
    }
}

#[test]
fn shareable_captures_survive_gc_after_channel_transfer() {
    let source = r#"
async fn produce(channel: Channel<fn() -> String>) {
    let text = "captured " + "string";
    channel.send(|| { gc_collect(); return text; });
}
async fn main() {
    let channel = Channel<fn() -> String>::new();
    await produce(channel);
    gc_collect();
    let f = channel.recv();
    gc_minor_collect();
    println(f());
}
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "captured string\n");
    }
}

#[test]
fn shareable_captures_cli_rejections_keep_capture_labels() {
    for (setup, operation) in [
        ("let a = [1]; let f = || a.len();", "let _ = run(f);"),
        ("let a = [1]; let f = || a.len();", "channel.send(f);"),
        (
            "let a = [1]; let f = || a.len();",
            "select { channel.send(f) => {} }",
        ),
        (
            "let mut a = 0; let f = || a; let writer = || { a = a + 1; };",
            "let _ = run(f); writer();",
        ),
    ] {
        let source = format!(
            "import std::collections::Array;\nasync fn run(f: fn() -> i64) -> i64 {{ return f(); }}\nfn main() {{\n{setup}\nlet channel = Channel<fn() -> i64>::new();\n{operation}\n}}"
        );
        let project = TestProject::new("shareable_capture_diagnostics", &[("main.wi", &source)]);
        // Repeat through the build cache as well as fresh typed-body queries.
        for _ in 0..2 {
            let out = project.compile("main.wi");
            let errors = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{source}");
            assert!(
                errors.contains("E2402") && errors.contains("captured variable `a`"),
                "{errors}"
            );
        }
    }
}

#[test]
fn shareable_captures_cli_reassignment_replaces_capture_labels() {
    for update in [
        "f = || b.len();",
        "if choice { f = || b.len(); } else { f = || b.len(); }",
        "match choice { true => { f = || b.len(); }, false => { f = || b.len(); } };",
    ] {
        for transfer in [
            "let _ = run(f);",
            "channel.send(f);",
            "select { channel.send(f) => {} }",
        ] {
            let source = format!(
                "import std::collections::Array;\nasync fn run(f: fn() -> i64) -> i64 {{ return f(); }}\nfn main() {{\nlet a = [1];\nlet b = [2];\nlet mut f = || a.len();\nlet choice = true;\n{update}\nlet channel = Channel<fn() -> i64>::new();\n{transfer}\n}}"
            );
            let project =
                TestProject::new("reassigned_capture_diagnostics", &[("main.wi", &source)]);
            for _ in 0..2 {
                let out = project.compile("main.wi");
                let errors = String::from_utf8_lossy(&out.stderr);
                assert!(!out.status.success(), "{source}");
                assert!(
                    errors.contains("E2402") && errors.contains("captured variable `b`"),
                    "{errors}"
                );
                assert!(!errors.contains("captured variable `a`"), "{errors}");
            }
        }
    }
}

#[test]
fn shareable_captures_cli_deferred_and_loop_exits_keep_causal_labels() {
    for (update, cause) in [
        ("defer { f = || b.len(); }", "a"),
        ("if true { defer { f = || b.len(); } }", "b"),
        ("if true { defer { f = one; } }", ""),
        ("while choice { f = || b.len(); return; }", "a"),
        ("for i in 0..2 { f = || b.len(); return; }", "a"),
        (
            "f = one; while true { if true { f = || b.len(); break; } else { f = one; } }",
            "b",
        ),
        (
            "f = one; for i in 0..2 { if true { f = || b.len(); break; } else { f = one; } }",
            "b",
        ),
        (
            "f = one; while choice { defer { f = || b.len(); } continue; }",
            "b",
        ),
        (
            "f = one; for i in 0..2 { defer { f = || b.len(); } continue; }",
            "b",
        ),
    ] {
        for transfer in [
            "let _ = run(f);",
            "channel.send(f);",
            "select { channel.send(f) => {} }",
        ] {
            let source = format!(
                "import std::collections::Array;\nasync fn run(f: fn() -> i64) -> i64 {{ return f(); }}\nfn one() -> i64 {{ return 1; }}\nfn main() {{\nlet a = [1];\nlet b = [2, 3];\nlet mut f = || a.len();\nlet choice = true;\n{update}\nlet channel = Channel<fn() -> i64>::new();\n{transfer}\n}}"
            );
            let project = TestProject::new("capture_exit_diagnostics", &[("main.wi", &source)]);
            for _ in 0..2 {
                let out = project.compile("main.wi");
                let errors = String::from_utf8_lossy(&out.stderr);
                assert!(!out.status.success(), "{source}");
                assert!(errors.contains("E2402"), "{source}: {errors}");
                if cause.is_empty() {
                    assert!(
                        !errors.contains("captured variable `"),
                        "{source}: {errors}"
                    );
                    continue;
                }
                assert!(
                    errors.contains(&format!("captured variable `{cause}`")),
                    "{source}: {errors}"
                );
                let obsolete = if cause == "a" { "b" } else { "a" };
                assert!(
                    !errors.contains(&format!("captured variable `{obsolete}`")),
                    "{source}: {errors}"
                );
            }
        }
    }
}

#[test]
fn shareable_captures_provenance_matches_native_cleanup_and_loop_order() {
    let source = r#"
import std::collections::Array;
fn main() {
    let a = [1];
    let b = [2, 3];
    let c = [4, 5, 6];
    let mut f = || a.len();
    if true {
        defer { f = || b.len(); }
        println(f());
    }
    println(f());
    while true {
        if true { f = || c.len(); break; }
    }
    println(f());
    for i in 0..1 { defer { f = || a.len(); } continue; }
    println(f());
    let mut g = f;
    if true { defer { f = g; } g = || b.len(); }
    println(f());
}
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "1\n2\n3\n1\n2\n");
    }
}

#[test]
fn shareable_captures_cli_match_and_call_effects_keep_causal_labels() {
    for (body, cause) in [
        (
            "match true { true if reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ => {} }; TRANSFER",
            "b",
        ),
        ("match f { g => { TRANSFER } };", "a"),
        (
            "match true { _ if let g = f => { TRANSFER }, _ => {} };",
            "a",
        ),
        ("match f { g if let h = g => { TRANSFER }, _ => {} };", "a"),
        (
            "let writer = || { f = || b.len(); }; writer(); TRANSFER",
            "b",
        ),
        (
            "let writer = || { f = || b.len(); }; let alias = writer; alias(); TRANSFER",
            "b",
        ),
        (
            "let writer = || { f = || b.len(); }; let wrapper = || { writer(); }; wrapper(); TRANSFER",
            "b",
        ),
        (
            "let writer = || { f = || b.len(); }; if true { defer writer(); } TRANSFER",
            "b",
        ),
        (
            "let writer = || { f = || b.len(); }; defer writer(); TRANSFER",
            "a",
        ),
        ("let writer = || { f = one; }; writer(); TRANSFER", ""),
        (
            "let writer = || { f = || b.len(); }; invoke(writer); TRANSFER",
            "",
        ),
    ] {
        let value = if body.contains("let h =") {
            "h"
        } else if body.contains("g =>") || body.contains("let g =") {
            "g"
        } else {
            "f"
        };
        for transfer in [
            format!("let _ = run({value});"),
            format!("channel.send({value});"),
            format!("select {{ channel.send({value}) => {{}} }}"),
        ] {
            let body = body.replace("TRANSFER", &transfer);
            let source = format!(
                "import std::collections::Array;\nasync fn run(f: fn() -> i64) -> i64 {{ return f(); }}\nfn one() -> i64 {{ return 1; }}\nfn reject(value: void) -> bool {{ return false; }}\nfn invoke(f: fn()) {{ f(); }}\nfn main() {{\nlet a = [1];\nlet b = [2, 3];\nlet mut f = || a.len();\nlet channel = Channel<fn() -> i64>::new();\n{body}\n}}"
            );
            let project = TestProject::new("capture_call_diagnostics", &[("main.wi", &source)]);
            for _ in 0..2 {
                let out = project.compile("main.wi");
                let errors = String::from_utf8_lossy(&out.stderr);
                assert!(
                    !out.status.success() && errors.contains("E2402"),
                    "{source}: {errors}"
                );
                if cause.is_empty() {
                    assert!(
                        !errors.contains("captured variable `"),
                        "{source}: {errors}"
                    );
                } else {
                    assert!(
                        errors.contains(&format!("captured variable `{cause}`")),
                        "{source}: {errors}"
                    );
                    let obsolete = if cause == "a" { "b" } else { "a" };
                    assert!(
                        !errors.contains(&format!("captured variable `{obsolete}`")),
                        "{source}: {errors}"
                    );
                }
            }
        }
    }
}

#[test]
fn shareable_captures_provenance_matches_native_match_and_call_order() {
    let source = r#"
import std::collections::Array;
fn reject(value: void) -> bool { return false; }
fn main() {
    let a = [1];
    let b = [2, 3];
    let mut f = || a.len();
    match true {
        true if reject(match true { true => { f = || b.len(); }, false => {} }) => {},
        _ => { println(f()); }
    };
    f = || a.len();
    match f { g if let h = g => { println(h()); }, _ => {} };
    let writer = || { f = || b.len(); };
    let wrapper = || { writer(); };
    wrapper();
    println(f());
    f = || a.len();
    if true { defer writer(); println(f()); }
    println(f());
    let writer_arg = |g: fn() -> i64| { f = g; };
    if true { defer writer_arg(f); f = || a.len(); }
    println(f());
}
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "2\n1\n2\n1\n2\n2\n");
    }
}
