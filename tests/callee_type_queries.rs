//! Callee source tokens report callable types without changing expression evidence.
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};

fn snapshot(source: &str, label: &str) -> Snapshot {
    let directory = std::env::temp_dir().join(format!(
        "willow-callee-types-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    std::fs::write(&path, source).unwrap();
    let result = CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    result
}

#[test]
fn callee_tokens_report_their_own_types() {
    let source = include_str!("../example/callee_type_queries.wi");
    let snapshot = snapshot(source, "tokens");
    let roundtrip: Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    for snapshot in [snapshot, roundtrip] {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (needle, expected) in [
            ("callback(value)", "closure(i64) -> f64"),
            ("function(value)", "fn(i64) -> i64"),
            ("increment(3)", "fn(i64) -> i64"),
            ("local(4)", "fn(i64) -> i64"),
            ("captured(5)", "closure(i64) -> i64"),
            ("mutable(6)", "fn(i64) -> i64"),
            ("increment(increment(7))", "fn(i64) -> i64"),
            ("increment(7)", "fn(i64) -> i64"),
            ("apply_closure(|", "fn(closure(i64) -> f64, i64) -> f64"),
            ("apply_function(local", "fn(fn(i64) -> i64, i64) -> i64"),
        ] {
            let start = source.find(needle).unwrap();
            let width = needle.find('(').unwrap();
            // First, middle and last bytes are all inside the same callee token.
            for offset in [0, width / 2, width - 1] {
                let result = session.query(QueryRequest::TypeAt {
                    revision: revision.clone(),
                    file: "main.wi".into(),
                    byte: start + offset,
                });
                assert_eq!(
                    result["result"]["type_display"], expected,
                    "{needle}+{offset}: {result}"
                );
            }
        }
    }
}

#[test]
fn non_callee_positions_and_expression_evidence_are_unchanged() {
    let source = include_str!("../example/callee_type_queries.wi");
    let snapshot = snapshot(source, "boundaries");
    let calls: Vec<_> = snapshot
        .semantic
        .references
        .iter()
        .filter(|r| r.role == "call")
        .map(|r| r.location.clone())
        .collect();
    assert!(calls.len() >= 10);
    // Reconstruct the previous index policy without altering expression evidence
    // or symbol resolution: read references were not given token priority.
    let mut baseline = snapshot.clone();
    for reference in &mut baseline.semantic.references {
        if reference.role == "call" {
            reference.role = "read".into();
        }
    }
    assert_eq!(snapshot.semantic.expressions, baseline.semantic.expressions);
    let revision = snapshot.revision.clone();
    // Reseal the deliberately altered fixture using the snapshot digest tuple.
    use sha2::{Digest, Sha256};
    baseline.revision = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                baseline.version,
                &baseline.compiler,
                &baseline.compatibility,
                &baseline.workspace,
                &baseline.sources,
                &baseline.functions,
                &baseline.semantic,
            ))
            .unwrap()
        )
    );
    let baseline_revision = baseline.revision.clone();
    let mut baseline = QuerySession::new(baseline).unwrap();
    let mut session = QuerySession::new(snapshot).unwrap();
    for byte in 0..=source.len() {
        if calls.iter().any(|l| l.start <= byte && byte < l.end) {
            continue;
        }
        let request = QueryRequest::TypeAt {
            revision: revision.clone(),
            file: "main.wi".into(),
            byte,
        };
        assert_eq!(
            session.query(request)["result"],
            baseline.query(QueryRequest::TypeAt {
                revision: baseline_revision.clone(),
                file: "main.wi".into(),
                byte
            })["result"],
            "byte {byte}"
        );
    }
}

#[test]
fn repeated_callee_queries_scale_logarithmically() {
    let mut previous = None;
    for n in [16usize, 64, 256] {
        let prefix = "fn identity(x: i64) -> i64 { return x; } fn main() {\n";
        let site = "println(identity(identity(1)));\n";
        let source = format!("{prefix}{}}}", site.repeat(n));
        let snapshot = snapshot(&source, &format!("scale-{n}"));
        let expressions = snapshot.semantic.expressions.len();
        let references = snapshot.semantic.references.len();
        if let Some((old_n, old_expressions, old_references)) = previous {
            assert_eq!(expressions - old_expressions, (n - old_n) * 4);
            assert_eq!(references - old_references, (n - old_n) * 3);
        }
        previous = Some((n, expressions, references));
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for repeats in [1, 8, 64] {
            let before = session.position_comparisons;
            for _ in 0..repeats {
                for i in 0..n {
                    for offset in [8, 17] {
                        let result = session.query(QueryRequest::TypeAt {
                            revision: revision.clone(),
                            file: "main.wi".into(),
                            byte: prefix.len() + i * site.len() + offset,
                        });
                        assert_eq!(
                            result["result"]["type_display"], "fn(i64) -> i64",
                            "{result}"
                        );
                    }
                }
            }
            let queries = repeats * n * 2;
            let comparisons = session.position_comparisons - before;
            assert!(comparisons <= queries * ((expressions + references).ilog2() as usize + 3));
            println!(
                "sites={n} expressions={expressions} references={references} queries={queries} position_comparisons={comparisons}"
            );
        }
    }
}
