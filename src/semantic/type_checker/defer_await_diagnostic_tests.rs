//! Deferred cleanup runs synchronously at scope exit, so it cannot wait for a
//! task. Each deferred `await`, `select` or async call is one mistake and gets
//! one error that names the async-cleanup alternative.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const PRELUDE: &str = "async fn bump(n: i64) -> i64 { return n + 1; }
async fn pair(a: i64, b: i64) -> i64 { return a + b; }
fn tidy(n: i64) { println(n); }
class Svc {
    pub n: i64;
    pub async fn stop(self) -> i64 { return self.n; }
    pub static async fn shutdown() -> i64 { return 0; }
}
";

const ASYNC_CALL: &str = "cannot `defer` an async call";
const AWAIT: &str = "cannot `defer` an `await`";
const SUSPEND: &str = "`await` and `select` are not allowed inside a `defer`";
const HELP: &str = "perform the async cleanup explicitly, or defer a synchronous helper";

fn check(items: &str) -> (String, Vec<Diagnostic>) {
    let source = format!("{PRELUDE}{items}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors = checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    (source, errors)
}

fn in_async_main(body: &str) -> (String, Vec<Diagnostic>) {
    check(&format!("async fn main() {{ {body} }}"))
}

/// `(message, covered text)` of each deferred-async error, in report order.
fn defer_errors(source: &str, errors: &[Diagnostic]) -> Vec<(String, String)> {
    errors
        .iter()
        .filter(|d| d.code == ErrorCode::E0905)
        .map(|d| {
            assert!(d.helps.iter().any(|h| h == HELP), "{d:?}");
            let span = d.primary_span().expect("primary span");
            (d.message.clone(), source[span.start..span.end].to_string())
        })
        .collect()
}

#[test]
fn deferred_await_reports_one_error_with_the_async_cleanup_help() {
    for (name, body, message) in [
        (
            "function call",
            "defer await bump(1); println(1);",
            ASYNC_CALL,
        ),
        (
            "call with async argument",
            "defer await pair(1, 2); println(1);",
            ASYNC_CALL,
        ),
        (
            "method call",
            "let s = new Svc(1); defer await s.stop(); println(1);",
            ASYNC_CALL,
        ),
        (
            "static call",
            "defer await Svc::shutdown(); println(1);",
            ASYNC_CALL,
        ),
        (
            "task variable",
            "let t = bump(1); defer await t; println(1);",
            AWAIT,
        ),
        (
            "inside a nested block",
            "if true { defer await bump(1); println(1); }",
            ASYNC_CALL,
        ),
        (
            "inside a loop",
            "for i in 0..2 { defer await bump(i); }",
            ASYNC_CALL,
        ),
    ] {
        let (source, errors) = in_async_main(body);
        assert_eq!(
            defer_errors(&source, &errors),
            [(message.to_string(), "await".to_string())],
            "{name}: {errors:?}"
        );
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    }
}

#[test]
fn deferred_await_in_a_sync_function_reports_only_the_defer() {
    // "make the function async" would not fix it: deferred cleanup still
    // could not wait.
    for items in [
        "fn sync() { defer await bump(1); } async fn main() { sync(); }",
        "fn sync() { let s = new Svc(1); defer await s.stop(); } async fn main() { sync(); }",
    ] {
        let (source, errors) = check(items);
        assert_eq!(
            defer_errors(&source, &errors),
            [(ASYNC_CALL.to_string(), "await".to_string())],
            "{items}: {errors:?}"
        );
        assert_eq!(errors.len(), 1, "{items}: {errors:?}");
    }
}

#[test]
fn awaited_calls_inside_a_deferred_block_report_the_suspension_once() {
    for (name, body) in [
        ("awaited call", "defer { await bump(3); }"),
        (
            "awaited call bound",
            "defer { let x = await bump(1); println(x); }",
        ),
        (
            "awaited method",
            "let s = new Svc(1); defer { await s.stop(); }",
        ),
        ("awaited task", "let t = bump(1); defer { await t; }"),
        ("two awaits", "defer { await bump(1); await bump(2); }"),
        (
            "await in a nested branch",
            "defer { if true { println(await bump(1)); } }",
        ),
    ] {
        let (source, errors) = in_async_main(body);
        let found = defer_errors(&source, &errors);
        assert_eq!(found.len(), 1, "{name}: {errors:?}");
        assert_eq!(found[0].0, SUSPEND, "{name}");
        assert!(found[0].1.starts_with('{'), "{name}: {found:?}");
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    }
}

#[test]
fn deferred_select_reports_one_error() {
    for body in [
        "let ch: Channel<i64> = Channel::new(); defer select { let v = ch.recv() => { println(v); } };",
        "let ch: Channel<i64> = Channel::new(); defer { select { let v = ch.recv() => { println(v); } } }",
    ] {
        let (source, errors) = in_async_main(body);
        let found = defer_errors(&source, &errors);
        assert_eq!(found.len(), 1, "{body}: {errors:?}");
        assert_eq!(found[0].0, SUSPEND, "{body}");
        assert_eq!(errors.len(), 1, "{body}: {errors:?}");
    }
}

#[test]
fn unawaited_async_calls_keep_their_own_error() {
    for (name, body, covered) in [
        ("direct call", "defer bump(4);", "bump"),
        ("call in a block", "defer { bump(4); }", "bump"),
        (
            "method in a block",
            "let s = new Svc(1); defer { s.stop(); }",
            "stop",
        ),
    ] {
        let (source, errors) = in_async_main(body);
        let found = defer_errors(&source, &errors);
        assert_eq!(found.len(), 1, "{name}: {errors:?}");
        assert_eq!(found[0].0, ASYNC_CALL, "{name}");
        assert!(found[0].1.contains(covered), "{name}: {found:?}");
    }
}

#[test]
fn separate_mistakes_are_each_reported() {
    for (body, expected) in [
        (
            "defer { await bump(1); bump(2); }",
            vec![SUSPEND, ASYNC_CALL],
        ),
        (
            "defer await bump(1); defer await bump(2); println(0);",
            vec![ASYNC_CALL, ASYNC_CALL],
        ),
        (
            "let t = bump(0); defer await t; defer bump(1); println(0);",
            vec![AWAIT, ASYNC_CALL],
        ),
    ] {
        let (source, errors) = in_async_main(body);
        let found: Vec<_> = defer_errors(&source, &errors)
            .into_iter()
            .map(|(m, _)| m)
            .collect();
        assert_eq!(found, expected, "{body}: {errors:?}");
    }
}

#[test]
fn operand_errors_are_still_reported_beside_the_defer() {
    let (source, errors) = in_async_main("defer await bump(missing); println(0);");
    assert_eq!(
        defer_errors(&source, &errors),
        [(ASYNC_CALL.to_string(), "await".to_string())],
        "{errors:?}"
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors.iter().any(|d| d.message.contains("missing")),
        "{errors:?}"
    );
}

#[test]
fn synchronous_cleanup_and_the_suggested_rewrite_are_accepted() {
    for body in [
        "defer tidy(1); println(0);",
        "defer { tidy(1); tidy(2); }",
        "println(await bump(1)); defer tidy(2);",
        "let t = bump(1); defer tidy(0); println(await t);",
    ] {
        let (_, errors) = in_async_main(body);
        assert!(errors.is_empty(), "{body}: {errors:?}");
    }
}

#[test]
fn many_deferred_awaits_report_one_error_each() {
    for size in [4usize, 64] {
        let mut body = String::new();
        for i in 0..size {
            body.push_str(&format!("defer await bump({i}); "));
        }
        let (source, errors) = in_async_main(&body);
        assert_eq!(defer_errors(&source, &errors).len(), size, "{size}");
        assert_eq!(errors.len(), size, "{size}: {errors:?}");
    }
}
