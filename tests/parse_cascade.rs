use std::fs;
use willow_compiler::diagnostics::{
    Diagnostic, DiagnosticEmitter, ErrorCode, source_map::SourceLookup,
};
use willow_compiler::{CompilerOptions, check_file};

#[derive(Default)]
struct Capture(Vec<Diagnostic>);
impl DiagnosticEmitter for Capture {
    fn emit(&mut self, d: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
        self.0.push(d.clone());
        Ok(())
    }
}

#[test]
fn parse_cascade_regressions() {
    let root = std::env::temp_dir().join(format!("willow-parse-cascade-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    // Ten malformed shapes in entry and imported files: twenty perspectives.
    for (case, source) in [
        "fn broken() { let x = ; } fn main() {}",
        "fn broken() { return 1 +; } fn main() {}",
        "fn broken() { if (true { return; } let a = 1; println(a); } fn main() {}",
        "fn broken() { if true {{ println(1); } } fn main() {}",
        "fn broken() { println(1);",
        "class Missing { fn open(self) {} } async fn main() { let x: Missing = new Missing(); }",
        "fn broken(x: Task<i64) {} async fn main() {}",
        "fn broken() { let x = 1 +; println(absent); } fn main() {}",
        "fn broken() { let x = ; } async fn main() { let x: Unit = absent; }",
        "fn broken() { if true { println(1); } pub class Later {} fn main() {}",
    ]
    .iter()
    .enumerate()
    {
        for imported in [false, true] {
            let entry = if imported {
                fs::write(root.join("broken.wi"), source).unwrap();
                "import broken; async fn main() { let x: broken::Missing = absent; }"
            } else {
                source
            };
            let path = root.join("main.wi");
            fs::write(&path, entry).unwrap();
            let mut capture = Capture::default();
            assert!(
                check_file(
                    path.to_str().unwrap(),
                    &CompilerOptions::debug(),
                    &mut capture
                )
                .is_err(),
                "case {case}, imported={imported}"
            );
            assert!(!capture.0.is_empty(), "case {case}");
            assert!(
                capture.0.len() <= 4,
                "case {case}, imported={imported}: {:?}",
                capture.0
            );
            assert!(
                capture.0.iter().all(|d| d.code.as_str().starts_with("E01")),
                "case {case}, imported={imported}: {:?}",
                capture.0
            );
        }
    }
    // Imported lexer errors and parser codes outside E01 must also stop semantics.
    for (source, code) in [
        ("pub fn broken() { @ }", ErrorCode::E0050),
        ("module broken; module broken;", ErrorCode::E2009),
    ] {
        fs::write(root.join("broken.wi"), source).unwrap();
        fs::write(
            root.join("main.wi"),
            "import broken; async fn main() { let x: Unit = absent; }",
        )
        .unwrap();
        let mut capture = Capture::default();
        assert!(
            check_file(
                root.join("main.wi").to_str().unwrap(),
                &CompilerOptions::debug(),
                &mut capture
            )
            .is_err()
        );
        assert_eq!(capture.0.len(), 1, "{source}: {:?}", capture.0);
        assert_eq!(capture.0[0].code, code);
    }
    for n in [16, 64, 256] {
        let path = root.join("main.wi");
        fs::write(
            &path,
            format!(
                "fn broken() {{ let x = ; {} }} fn main() {{}}",
                "println(absent);".repeat(n)
            ),
        )
        .unwrap();
        let mut capture = Capture::default();
        assert!(
            check_file(
                path.to_str().unwrap(),
                &CompilerOptions::debug(),
                &mut capture
            )
            .is_err()
        );
        assert_eq!(capture.0.len(), 1, "n={n}: {:?}", capture.0);
    }
    // A valid program still receives real semantic errors.
    fs::write(
        root.join("main.wi"),
        "async fn main() { let x: Missing = absent; }",
    )
    .unwrap();
    let mut capture = Capture::default();
    assert!(
        check_file(
            root.join("main.wi").to_str().unwrap(),
            &CompilerOptions::debug(),
            &mut capture
        )
        .is_err()
    );
    assert!(capture.0.iter().any(|d| d.code == ErrorCode::E0350));
    assert!(
        !capture.0.iter().any(|d| d.code == ErrorCode::E2402),
        "{:?}",
        capture.0
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unclosed_block_points_to_opening_brace() {
    for source in ["fn main() {", "fn bad() { println(1); pub class Later {}"] {
        let tokens = willow_compiler::lexer::Lexer::new(source)
            .tokenize()
            .unwrap();
        let (_, errors) = willow_compiler::parser::Parser::new(tokens).parse();
        let d = errors.iter().find(|d| d.code == ErrorCode::E0103).unwrap();
        assert!(
            d.labels
                .iter()
                .any(|l| l.span.start == source.find('{').unwrap()
                    && l.message == "block opened here"),
            "{errors:?}"
        );
        assert!(!d.helps.iter().any(|h| h.contains("rename")));
    }
}
