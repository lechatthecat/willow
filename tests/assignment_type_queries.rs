//! Assignment source tokens must not inherit synthetic lowering types.
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};

fn snapshot(source: &str, label: &str) -> Snapshot {
    let directory = std::env::temp_dir().join(format!(
        "willow-assignment-types-{}-{label}",
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
fn assignment_types_cover_operators_places_and_snapshot_roundtrip() {
    let mut source = String::from(
        "class Counter { pub n: i64; pub init(self) { self.n = 8; } pub fn bump(self) {\n",
    );
    let mut cases = Vec::new();
    let mut add = |source: &mut String, marked: &str, ty: &str| {
        let offset = marked.find('@').unwrap();
        cases.push((source.len() + offset, ty.to_owned(), marked.to_owned()));
        source.push_str(&marked.replace('@', ""));
        source.push('\n');
    };
    for op in ["=", "+=", "-=", "*=", "/=", "%="] {
        add(&mut source, &format!("self.@n {op} 2;"), "i64");
    }
    source.push_str("} } fn main() { let mut n = 8; let xs = [8]; let grid = [[8]]; let i = 0; let c = new Counter();\n");
    for op in ["=", "+=", "-=", "*=", "/=", "%="] {
        for marked in [
            format!("@n {op} 2;"),
            format!("c.@n {op} 2;"),
            format!("xs@[i] {op} 2;"),
            format!("grid[i]@[i] {op} 2;"),
        ] {
            add(&mut source, &marked, "i64");
        }
    }
    source.push_str("let mut real = 1.5; let mut flag = true; let mut text = \"a\";\n");
    for (marked, ty) in [
        ("@real += 2.5;", "f64"),
        ("@flag = false;", "bool"),
        ("@text = \"b\";", "String"),
        ("xs[@i] += 1;", "i64"),
        ("n += @i;", "i64"),
        ("println(@n);", "i64"),
    ] {
        add(&mut source, marked, ty);
    }
    source.push('}');
    assert_eq!(cases.len(), 36);
    let original = snapshot(&source, "perspectives");
    let roundtrip: Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    for snapshot in [original, roundtrip] {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (byte, expected, marked) in &cases {
            let result = session.query(QueryRequest::TypeAt {
                revision: revision.clone(),
                file: "main.wi".into(),
                byte: *byte,
            });
            assert_eq!(result["result"]["status"], "ok", "{marked}: {result}");
            assert_eq!(
                result["result"]["type_display"], *expected,
                "{marked}: {result}"
            );
        }
    }
}

#[test]
fn assignment_type_queries_scale_logarithmically() {
    let mut previous = None;
    for n in [16usize, 64, 256] {
        let prefix = "fn main() { let mut n = 0; let xs = [0];\n";
        let site = "n += 1; xs[0] = n;\n";
        let source = format!("{prefix}{}}}", site.repeat(n));
        let snapshot = snapshot(&source, &format!("scale-{n}"));
        let expressions = snapshot.semantic.expressions.len();
        if let Some((old_n, old_expressions)) = previous {
            assert_eq!(expressions - old_expressions, (n - old_n) * 7);
        }
        previous = Some((n, expressions));
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for repeats in [1, 8, 64] {
            let before = session.position_comparisons;
            for _ in 0..repeats {
                for i in 0..n {
                    for offset in [0, 10] {
                        let result = session.query(QueryRequest::TypeAt {
                            revision: revision.clone(),
                            file: "main.wi".into(),
                            byte: prefix.len() + i * site.len() + offset,
                        });
                        assert_eq!(result["result"]["type_display"], "i64", "{result}");
                    }
                }
            }
            let queries = repeats * n * 2;
            let comparisons = session.position_comparisons - before;
            assert!(comparisons <= queries * (expressions.ilog2() as usize + 2) * 2);
            println!(
                "sites={n} expressions={expressions} queries={queries} position_comparisons={comparisons}"
            );
        }
    }
}
