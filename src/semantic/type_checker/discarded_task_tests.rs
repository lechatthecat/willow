use super::{ErrorCode, check_source};
use crate::diagnostics::Severity;

#[test]
fn discarded_task_warning_cases() {
    for body in [
        "work();",
        "sleep(1);",
        "yield();",
        "(work());",
        "if true { work(); }",
        "while false { work(); }",
        "let task = work(); task;",
        "true ? work() : work();",
        "work(); work();",
        "let task = work(); task.result();",
        "let s = TaskScope::new(); let t = s.add(work()); t; await s.finish();",
        "let s = TaskScope::new(); s.finish();",
    ] {
        let source = format!("async fn work() -> i64 {{ return 1; }} async fn main() {{ {body} }}");
        let diagnostics = check_source(&source);
        assert!(!diagnostics.is_empty(), "missing warning: {source}");
        for diagnostic in &diagnostics {
            assert_eq!(
                diagnostic.severity,
                Severity::Warning,
                "{source}: {diagnostics:?}"
            );
            assert_eq!(diagnostic.code, ErrorCode::W0802);
            assert!(diagnostic.primary_span().is_some());
            assert!(
                diagnostic
                    .helps
                    .iter()
                    .any(|help| help.contains("await ...") && help.contains("let _"))
            );
        }
        assert_eq!(
            diagnostics.len(),
            if body == "work(); work();" { 2 } else { 1 },
            "{source}"
        );
    }
}

#[test]
fn discarded_task_consumed_values_are_quiet() {
    for body in [
        "await work();",
        "await sleep(1);",
        "await yield();",
        "let task = work(); await task;",
        "let task = sleep(1); await task;",
        "let _ = work();",
        "let detached = work();",
        "let value = await work();",
        "println(await work());",
        "1;",
        "plain();",
        "let value = 1; value;",
        "if true { await work(); }",
        "while false { await work(); }",
        "let s = TaskScope::new(); s.add(work()); await s.finish();",
        "let s = TaskScope::new(); let c = s.child(); c.add(work()); await s.finish();",
        "let s = TaskScope::new(); if true { s.add(work()); } await s.finish();",
    ] {
        let source = format!(
            "fn plain() -> i64 {{ return 1; }} async fn work() -> i64 {{ return 1; }} async fn main() {{ {body} }}"
        );
        let diagnostics = check_source(&source);
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
    }
}

#[test]
fn discarded_task_call_forms_and_value_contexts() {
    for (source, expected) in [
        ("async fn work() {} fn main() { work(); }", 1),
        (
            "async fn work<T>(value: T) -> T { return value; } async fn main() { work(1); }",
            1,
        ),
        (
            "class C { pub async fn work(self) -> i64 { return 1; } } async fn main() { let c = new C(); c.work(); }",
            1,
        ),
        (
            "async fn work() -> i64 { return 1; } fn factory() -> Task<i64> { return work(); } async fn main() { factory(); }",
            1,
        ),
        (
            "async fn work() -> i64 { return 1; } fn factory() -> Task<i64> { return work(); } async fn main() { await factory(); }",
            0,
        ),
        (
            "async fn work() -> i64 { return 1; } async fn main() { let task = match true { true => work(), false => work() }; await task; }",
            0,
        ),
        (
            "async fn work() -> i64 { return 1; } async fn main() { match true { true => work(), false => work() }; }",
            1,
        ),
        (
            "async fn work() -> i64 { return 1; } async fn main() { let task = work(); await task.result(); }",
            0,
        ),
        (
            "async fn work() -> i64 { return 1; } fn consume(task: Task<i64>) {} async fn main() { consume(work()); }",
            0,
        ),
    ] {
        let diagnostics = check_source(source);
        assert_eq!(diagnostics.len(), expected, "{source}: {diagnostics:?}");
        assert!(
            diagnostics.iter().all(|d| d.code == ErrorCode::W0802),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn discarded_task_diagnostics_scale_with_statements() {
    for count in [1, 8, 64, 256] {
        let source = format!(
            "async fn work() -> i64 {{ return 1; }} async fn main() {{ {} }}",
            "work();".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count);
        for (index, diagnostic) in diagnostics.iter().enumerate() {
            assert_eq!(diagnostic.code, ErrorCode::W0802);
            let span = diagnostic.primary_span().unwrap();
            assert_eq!(&source[span.start..span.end], "work");
            if index > 0 {
                assert!(span.start > diagnostics[index - 1].primary_span().unwrap().start);
            }
        }
    }
}

#[test]
fn discarded_task_invalid_calls_do_not_add_warnings() {
    for body in ["sleep(true);", "sleep();", "missing();", "work(1);"] {
        let source = format!("async fn work() {{}} async fn main() {{ {body} }}");
        let diagnostics = check_source(&source);
        assert!(diagnostics.iter().any(|d| d.severity == Severity::Error));
        assert!(
            !diagnostics.iter().any(|d| d.code == ErrorCode::W0802),
            "{diagnostics:?}"
        );
    }
}
