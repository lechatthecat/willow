//! Callee token types for builtin and source functions/methods.
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};
const SOURCE: &str = include_str!("../example/builtin_callee_type_queries.wi");
fn snapshot(files: &[(&str, &str)], label: &str) -> Snapshot {
    let directory = std::env::temp_dir().join(format!(
        "willow-builtin-callee-types-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    for (name, source) in files {
        std::fs::write(directory.join(name), source).unwrap();
    }
    let path = directory.join("main.wi");
    let result = CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    result
}

/// Byte of `token` inside the `occurrence`-th match of `needle`.
fn byte(source: &str, needle: &str, occurrence: usize, token: &str) -> usize {
    let start = source
        .match_indices(needle)
        .nth(occurrence)
        .unwrap_or_else(|| panic!("{needle} #{occurrence}"))
        .0;
    start + needle.find(token).unwrap()
}

fn type_at(
    session: &mut QuerySession,
    revision: &str,
    file: &str,
    byte: usize,
) -> serde_json::Value {
    session.query(QueryRequest::TypeAt {
        revision: revision.into(),
        file: file.into(),
        byte,
    })
}

fn both(snapshot: Snapshot) -> [Snapshot; 2] {
    let roundtrip: Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    [snapshot, roundtrip]
}

#[test]
fn callee_tokens_report_signatures() {
    for snapshot in both(snapshot(&[("main.wi", SOURCE)], "callee-signatures")) {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (needle, token, expected) in [
            ("= text()", "text", "fn() -> String"),
            ("label.text()", "text", "fn() -> String"),
            ("text().split", "split", "fn(String) -> Array<String>"),
            ("s.len()", "len", "fn() -> i64"),
            ("s.trim()", "trim", "fn() -> String"),
            ("s.substring", "substring", "fn(i64, i64) -> String"),
            ("s.slice", "slice", "fn(i64, i64) -> String"),
            ("s.repeat", "repeat", "fn(i64) -> String"),
            ("s.byte_at", "byte_at", "fn(i64) -> i64"),
            ("s.contains", "contains", "fn(String) -> bool"),
            ("s.find", "find", "fn(String) -> i64"),
            ("s.starts_with", "starts_with", "fn(String) -> bool"),
            ("n.toString", "toString", "fn() -> String"),
            ("n.wrapping_add", "wrapping_add", "fn(i64) -> i64"),
            ("n.wrapping_neg", "wrapping_neg", "fn() -> i64"),
            ("n.checked_add", "checked_add", "fn(i64) -> Option<i64>"),
            ("parts.len", "len", "fn() -> i64"),
            ("parts[0].trim", "trim", "fn() -> String"),
            ("trim().len", "len", "fn() -> i64"),
            ("sleep(0)", "sleep", "fn(i64) -> Future<void>"),
            ("yield()", "yield", "fn() -> Future<void>"),
            ("print(1)", "print", "fn(i64) -> void"),
            ("println(t)", "println", "fn(String) -> void"),
        ] {
            let start = byte(SOURCE, needle, 0, token);
            for offset in 0..token.len() {
                let result = type_at(&mut session, &revision, "main.wi", start + offset);
                assert_eq!(
                    result["result"]["type_display"], expected,
                    "{needle}+{offset}: {result}"
                );
            }
            let result = type_at(&mut session, &revision, "main.wi", start + token.len());
            assert_ne!(
                result["result"]["type_display"], expected,
                "parenthesis: {needle}: {result}"
            );
        }
    }
}

#[test]
fn builtin_callee_evidence_scales_with_calls_and_chains() {
    for n in [1, 16, 64] {
        for chained in [false, true] {
            let body = if chained {
                format!("let x = \"x\"{};", ".trim()".repeat(n))
            } else {
                "\"x\".trim();".repeat(n)
            };
            let source = format!("fn main() {{ {body} }}");
            let snapshot = snapshot(&[("main.wi", &source)], &format!("scale-{n}-{chained}"));
            let typed: Vec<_> = snapshot
                .semantic
                .references
                .iter()
                .filter(|r| r.ty.is_some() && !r.location.path.starts_with("<std>"))
                .collect();
            assert_eq!(typed.len(), n);
            assert!(typed.iter().all(|r| r.role == "call"));
            println!(
                "calls={n} chained={chained} typed_references={}",
                typed.len()
            );
            let revision = snapshot.revision.clone();
            let mut session = QuerySession::new(snapshot).unwrap();
            let at = source.rfind("trim").unwrap();
            let result = type_at(&mut session, &revision, "main.wi", at);
            assert_eq!(
                result["result"]["type_display"], "fn() -> String",
                "{result}"
            );
        }
    }
}

#[test]
fn builtin_arguments_receivers_and_contextual_callbacks_keep_their_types() {
    let source = r#"fn main() {
        let s = "split";
        let parts = s.split("split");
        let value: Option<i64> = Some(1);
        let mapped = value.map(|x: i64| x + 1);
        let fallback = mapped.unwrap_or(2);
        println(fallback);
    }"#;
    let snapshot = snapshot(&[("main.wi", source)], "arguments");
    let revision = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot).unwrap();
    for (needle, token, expected) in [
        ("s.split", "s", "String"),
        ("split(\"split\")", "\"split\"", "String"),
        ("value.map", "map", "fn(fn(i64) -> i64) -> Option<i64>"),
        ("mapped.unwrap_or", "unwrap_or", "fn(i64) -> i64"),
        ("unwrap_or(2)", "2", "i64"),
    ] {
        let result = type_at(
            &mut session,
            &revision,
            "main.wi",
            byte(source, needle, 0, token),
        );
        assert_eq!(
            result["result"]["type_display"], expected,
            "{needle}: {result}"
        );
    }
}

#[test]
fn builtin_static_callee_tokens_and_boundaries_roundtrip() {
    let source = r#"class Parser {
        pub static fn parse(s: String) -> String { return s; }
    }
    enum Choice { Value(i64) }
    fn main() {
        let integer = i64::parse("+42");
        let float = f64::parse("1.5");
        let cell = AtomicI64::new(1);
        let channel = Channel<i64>::with_capacity(2);
        let source = Parser::parse("parse");
        let variant = Choice::Value(3);
        let nested = AtomicI64::new(i64::parse("42").unwrap());
    }"#;
    for snapshot in both(snapshot(&[("main.wi", source)], "static-callees")) {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (needle, token, expected) in [
            (
                "i64::parse(\"+42",
                "parse",
                "fn(String) -> Result<i64, ParseIntError>",
            ),
            (
                "f64::parse",
                "parse",
                "fn(String) -> Result<f64, ParseFloatError>",
            ),
            ("AtomicI64::new(1)", "new", "fn(i64) -> AtomicI64"),
            (
                "Channel<i64>::with_capacity",
                "with_capacity",
                "fn(i64) -> Channel<i64>",
            ),
            ("Parser::parse", "parse", "fn(String) -> String"),
            ("AtomicI64::new(i64", "new", "fn(i64) -> AtomicI64"),
            (
                "i64::parse(\"42",
                "parse",
                "fn(String) -> Result<i64, ParseIntError>",
            ),
        ] {
            let at = byte(source, needle, 0, token);
            for offset in 0..token.len() {
                let result = type_at(&mut session, &revision, "main.wi", at + offset);
                assert_eq!(
                    result["result"]["type_display"], expected,
                    "{needle}: {result}"
                );
            }
            for outside in [at - 1, at + token.len()] {
                let result = type_at(&mut session, &revision, "main.wi", outside);
                assert_ne!(
                    result["result"]["type_display"], expected,
                    "boundary {needle}: {result}"
                );
            }
        }
        let result = type_at(
            &mut session,
            &revision,
            "main.wi",
            byte(source, "Choice::Value", 0, "Value"),
        );
        assert!(
            !result["result"]["type_display"]
                .as_str()
                .unwrap_or("")
                .starts_with("fn("),
            "{result}"
        );
    }
}

#[test]
fn builtin_static_callee_evidence_scales_with_calls() {
    for n in [1, 16, 64] {
        let source = format!("fn main() {{ {} }}", "AtomicI64::new(1);".repeat(n));
        let snapshot = snapshot(&[("main.wi", &source)], &format!("static-scale-{n}"));
        let typed: Vec<_> = snapshot
            .semantic
            .references
            .iter()
            .filter(|r| r.ty.is_some() && !r.location.path.starts_with("<std>"))
            .collect();
        assert_eq!(typed.len(), n);
        assert!(typed.iter().all(|r| r.role == "call"));
        println!("static_calls={n} typed_references={}", typed.len());
    }
}

#[test]
fn contextual_builtin_static_callees_report_final_return_types() {
    let source = r#"import std::collections::Map;
    fn take(m: Map<String, i64>, c: Channel<String>) {}
    fn make() -> Map<String, i64> { return Map::new(); }
    fn main() {
        let m: Map<String, i64> = Map::new();
        let c: Channel<String> = Channel::new();
        let bounded: Channel<i64> = Channel::with_capacity(2);
        take(Map::new(), Channel::new());
        let explicit = Channel<i64>::new();
    }"#;
    for snapshot in both(snapshot(&[("main.wi", source)], "contextual-static")) {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (needle, token, expected) in [
            ("return Map::new", "new", "fn() -> Map<String, i64>"),
            ("= Map::new", "new", "fn() -> Map<String, i64>"),
            ("= Channel::new", "new", "fn() -> Channel<String>"),
            (
                "Channel::with_capacity",
                "with_capacity",
                "fn(i64) -> Channel<i64>",
            ),
            ("take(Map::new", "new", "fn() -> Map<String, i64>"),
            (", Channel::new", "new", "fn() -> Channel<String>"),
            ("Channel<i64>::new", "new", "fn() -> Channel<i64>"),
        ] {
            let at = byte(source, needle, 0, token);
            for offset in 0..token.len() {
                let result = type_at(&mut session, &revision, "main.wi", at + offset);
                assert_eq!(
                    result["result"]["type_display"], expected,
                    "{needle}: {result}"
                );
            }
        }
    }
}

#[test]
fn contextual_static_signature_finalization_keeps_one_reference_per_call() {
    for n in [1, 16, 64] {
        let body: String = (0..n)
            .map(|i| format!("let c{i}: Channel<String> = Channel::new();"))
            .collect();
        let source = format!("fn main() {{ {body} }}");
        let snapshot = snapshot(&[("main.wi", &source)], &format!("contextual-scale-{n}"));
        let typed: Vec<_> = snapshot
            .semantic
            .references
            .iter()
            .filter(|r| r.ty.is_some() && !r.location.path.starts_with("<std>"))
            .collect();
        assert_eq!(typed.len(), n);
        assert!(typed.iter().all(|r| r.role == "call"));
        println!("contextual_calls={n} typed_references={}", typed.len());
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (at, _) in source.match_indices("new") {
            let result = type_at(&mut session, &revision, "main.wi", at);
            assert_eq!(result["result"]["type_display"], "fn() -> Channel<String>");
        }
    }
}
