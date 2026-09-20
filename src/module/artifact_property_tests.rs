//! Deterministic generated properties for the actual on-disk body store.
use super::*;
use std::fmt::Write as _;

fn parse(source: &str, file: FileId) -> Program {
    let tokens = crate::lexer::Lexer::with_file_id(source, file)
        .tokenize()
        .unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}\n{source}");
    program
}

// Linear-size chains, wide bodies, repeated expressions, and Unicode payloads.
// No dependency on random state: the case number completely reproduces a source.
fn source(case: usize, owners: usize, depth: usize) -> String {
    let mut source = String::new();
    for owner in 0..owners {
        let value = case * 100 + owner;
        let mut body = format!("let mut x = {value};");
        for _ in 0..depth {
            body.push_str("if true {");
        }
        match case % 4 {
            0 => body.push_str("let f = |n: i64| { return n + 1; }; x = f(x);"),
            1 => body.push_str("let a = [1, 2, 3]; x = a[0]; defer { println(x); }"),
            2 => body.push_str("x = match true { true => 1, false => 2 };"),
            _ => body.push_str("println(\"日本語 🦀 \\n \\\"\"); while false { break; }"),
        }
        for _ in 0..depth {
            body.push('}');
        }
        for _ in 0..case % 5 {
            body.push_str("x = x + 1;");
        }
        // Five separately stored artifacts per group. Nested syntax belongs to
        // its enclosing body, and must not become extra top-level records.
        writeln!(source, "fn f{owner}() {{ {body} }}").unwrap();
        writeln!(
            source,
            "class C{owner} {{
                x: i64;
                pub static value: i64 = {value} + 1;
                pub init(self) {{ self.x = {value}; {body} }}
                pub fn get(self) -> i64 {{ {body} return self.x; }}
            }}
            interface I{owner} {{
                fn required(self) -> i64;
                fn defaultValue(self) -> i64 {{ {body} return {value}; }}
            }}"
        )
        .unwrap();
    }
    source
}

#[test]
fn generated_body_roundtrips_preserve_identity_and_reuse_records() {
    for case in 0..32 {
        let mut artifacts = UnitArtifacts::new().unwrap();
        let owners = 1 + case % 4;
        let mut summary = parse(&source(case, owners, case % 8), FileId(17));
        let expected = serde_json::to_vec(&summary).unwrap();
        artifacts.offload(&mut summary).unwrap();
        let shell = serde_json::to_vec(&summary).unwrap();
        assert_eq!(artifacts.next, 5 * owners, "case {case}");
        for _ in 0..3 {
            let restored = artifacts.hydrate(&summary, FileId(17)).unwrap();
            assert_eq!(
                serde_json::to_vec(&*restored).unwrap(),
                expected,
                "case {case}"
            );
            // Offloading an already stripped shell must not overwrite its bodies.
            artifacts.offload(&mut summary).unwrap();
            assert_eq!(serde_json::to_vec(&summary).unwrap(), shell);
            // Nor should a fresh hydrated copy allocate duplicate artifacts.
            let mut copy = (*restored).clone();
            artifacts.offload(&mut copy).unwrap();
            assert_eq!(serde_json::to_vec(&copy).unwrap(), shell);
            assert_eq!(artifacts.next, 5 * owners, "case {case}");
        }
        assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
    }
}

#[test]
fn generated_multifile_roundtrips_have_exact_linear_record_counts() {
    for files in [1, 4, 16] {
        for owners in [1, 4, 16] {
            let mut artifacts = UnitArtifacts::new().unwrap();
            let mut summaries = Vec::new();
            for index in 0..files {
                let file = FileId(index as u32);
                // Equal source offsets across files must not alias. Values vary
                // by file, while identifier spellings deliberately repeat.
                let source = source(index + 10, owners, 2);
                let mut summary = parse(&source, file);
                let expected = serde_json::to_vec(&summary).unwrap();
                artifacts.snapshot_source(file, &source).unwrap();
                artifacts
                    .snapshot_source(file, "replacement must be ignored")
                    .unwrap();
                artifacts.offload(&mut summary).unwrap();
                summaries.push((file, source, summary, expected));
            }
            assert_eq!(artifacts.blocks.len(), files * owners * 4);
            assert_eq!(artifacts.initializers.len(), files * owners);
            assert_eq!(artifacts.next, files * (5 * owners + 1));
            // Reverse order detects accidental dependence on the most recently
            // parsed program or sequential artifact reads.
            for (file, source, summary, expected) in summaries.iter().rev() {
                let restored = artifacts.hydrate(summary, *file).unwrap();
                assert_eq!(serde_json::to_vec(&*restored).unwrap(), *expected);
                assert_eq!(artifacts.source(*file).unwrap(), *source);
            }
            assert_eq!(artifacts.peaks(), [1, 0, 0, 0]);
            assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
        }
    }
}

#[test]
fn malformed_body_artifacts_return_errors_and_release_storage() {
    for bytes in [b"".as_slice(), b"{", b"null", b"{}", b"\xff"] {
        let directory;
        {
            let mut artifacts = UnitArtifacts::new().unwrap();
            directory = artifacts.directory.clone();
            let mut summary = parse("fn f() { println(42); }", FileId(3));
            artifacts.offload(&mut summary).unwrap();
            let id = *artifacts.blocks.values().next().unwrap();
            std::fs::write(directory.join(id.to_string()), bytes).unwrap();
            assert!(artifacts.hydrate(&summary, FileId(3)).is_err());
            assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
        }
        assert!(!directory.exists());
    }
}
