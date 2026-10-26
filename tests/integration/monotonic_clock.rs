use super::support::compile_and_run_with_env;

#[test]
fn monotonic_clock_language_paths() {
    let cases = [
        ("", "println(time::now_ns() >= 0);"),
        ("import std::time;", "println(time::now_ns() >= 0);"),
        (
            "import std::time as clock;",
            "println(clock::now_ns() >= 0);",
        ),
        ("import std::time::now_ns;", "println(time::now_ns() >= 0);"),
        (
            "import std::time::now_ns as stamp;",
            "println(time::now_ns() >= 0);",
        ),
        (
            "fn stamp() -> i64 { return time::now_ns(); }",
            "println(stamp() >= 0);",
        ),
        (
            "fn valid(n: i64) -> bool { return n >= 0; }",
            "println(valid(time::now_ns()));",
        ),
        (
            "",
            "let a = time::now_ns(); let b = time::now_ns(); println(b >= a);",
        ),
        (
            "",
            "let a = time::now_ns(); println(time::now_ns() - a >= 0);",
        ),
        (
            "",
            "let mut a = time::now_ns(); let mut i = 0; while i < 32 { let b = time::now_ns(); if b < a { panic(\"backwards\"); } a = b; i = i + 1; } println(true);",
        ),
        (
            "",
            "let a = if true { time::now_ns() } else { 0 }; println(a >= 0);",
        ),
        ("", "time::now_ns(); println(true);"),
        (
            "",
            "let a: i64 = time::now_ns(); let b = a; println(a == b);",
        ),
        (
            "fn now_ns() -> i64 { return 42; }",
            "println(now_ns() == 42);",
        ),
    ];
    for (index, (prefix, body)) in cases.iter().enumerate() {
        let source = format!("{prefix} fn main() {{ {body} }}");
        let (out, ok) = compile_and_run_with_env(&source, &[]);
        assert!(ok, "case {index}: {out}");
        assert_eq!(out, "true\n", "case {index}");
    }
}

#[test]
fn monotonic_clock_across_await_and_gc() {
    let source = r#"
import std::time as clock;
async fn main() {
    let start = clock::now_ns();
    let text = "still alive";
    await sleep(2);
    println(clock::now_ns() - start >= 2000000);
    println(text);
}
"#;
    for env in [vec![], vec![("WILLOW_GC_STRESS", "alloc")]] {
        let (out, ok) = compile_and_run_with_env(source, &env);
        assert!(ok, "{out}");
        assert_eq!(out, "true\nstill alive\n");
    }
}

#[test]
fn monotonic_clock_codegen_scaling() {
    use super::support::compile_and_collect_relocation_targets_mode;
    for release in [false, true] {
        for count in [1, 16, 256] {
            let source = format!("fn main() {{ {} }}", "time::now_ns();".repeat(count));
            let calls = compile_and_collect_relocation_targets_mode(&source, &[], release)
                .iter()
                .filter(|name| name.as_str() == "willow_time_now_ns")
                .count();
            assert_eq!(calls, count);
            println!("clock release={release} expressions={count} emitted_calls={calls}");
        }
    }
}

#[test]
fn monotonic_clock_example_runs() {
    let (out, ok) = super::support::compile_file_and_run("example/monotonic_clock.wi");
    assert!(ok, "{out}");
    assert!(out.starts_with("Elapsed nanoseconds:\n"));
    assert!(out.ends_with("\ntrue\n"));
}

#[test]
fn monotonic_clock_rejects_invalid_calls() {
    use super::support::TestProject;
    for source in [
        "fn main() { time::now_ns(1); }",
        "fn main() { let x: String = time::now_ns(); println(x); }",
        "import std::time::missing; fn main() {}",
        "fn main() { time::missing(); }",
    ] {
        let project = TestProject::new("clock_invalid", &[("main.wi", source)]);
        let output = project.compile("main.wi");
        assert!(
            !output.status.success(),
            "invalid program accepted: {source}"
        );
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(!errors.contains("internal compiler error"), "{errors}");
    }
}

#[test]
fn monotonic_clock_preserves_user_module() {
    let (out, ok) = super::support::compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import time; fn main() { println(time::now_ns()); }",
            ),
            ("time.wi", "pub fn now_ns() -> i64 { return 42; }"),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}
