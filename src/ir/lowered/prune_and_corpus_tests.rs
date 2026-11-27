use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;

fn lir(src: &str) -> SourceProgram {
    let tokens = Lexer::new(src).tokenize().expect("lexing failed");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "parse errors: {errs:?}");
    let (hir, diags) = super::super::lower::lower_program(&program);
    assert!(diags.is_empty(), "HIR diagnostics: {diags:?}");
    lower_source_program(&hir)
}

// 23. dead blocks after mid-block returns are pruned
#[test]
fn l23_dead_blocks_pruned() {
    let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
    let f = &p.functions[0];
    // Reachable shape: entry(branch) + then(return) + merge(return) = 3.
    assert_eq!(f.blocks.len(), 3, "{f:#?}");
    // Every edge stays in range after renumbering.
    for b in &f.blocks {
        match &b.terminator {
            SourceTerminator::Jump(t) => assert!(t.0 < f.blocks.len()),
            SourceTerminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                assert!(then_block.0 < f.blocks.len());
                assert!(else_block.0 < f.blocks.len());
            }
            SourceTerminator::Suspend { resume, .. } => assert!(resume.0 < f.blocks.len()),
            SourceTerminator::Return(_) | SourceTerminator::CleanupReturn => {}
        }
    }
}

// 24. block ids stay dense and self-consistent after pruning
#[test]
fn l24_pruned_ids_dense() {
    let p = lir("fn f(n: i64) -> i64 { if n > 0 { return 1; } if n < 0 { return -1; } return 0; }");
    let f = &p.functions[0];
    for (i, b) in f.blocks.iter().enumerate() {
        assert_eq!(b.id.0, i, "ids must be dense positions");
    }
}

// 25. corpus: every example and regression program parses and survives HIR→LIR lowering
// without panicking (coverage diagnostics are allowed; crashes are not).
#[test]
fn l25_examples_corpus_lowers_without_panic() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    for corpus in ["example", "tests/regressions"] {
        collect_wi_files(&root.join(corpus), &mut sources);
    }
    assert!(
        sources.len() > 30,
        "expected a real corpus, got {sources:?}"
    );

    let mut fully_covered = 0usize;
    for path in &sources {
        let text = std::fs::read_to_string(path).unwrap();
        let Ok(tokens) = Lexer::new(&text).tokenize() else {
            continue; // lexer-error fixtures are out of scope here
        };
        let (program, parse_errors) = Parser::new(tokens).parse();
        if !parse_errors.is_empty() {
            continue;
        }
        // Measure with the checker's side tables, as production lowering
        // does (checker errors are fine — import-using files won't fully
        // resolve here, and panic-safety is the primary assertion).
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude registers");
        checker.check_program(&program);
        let tables = super::super::lower::SemanticFacts::from_checker(&checker);
        let (hir, diags) = super::super::lower::lower_program_with(&program, &tables);
        let _ = lower_source_program(&hir); // must not panic
        if diags.is_empty() {
            fully_covered += 1;
        }
    }
    // A healthy majority of the real examples should lower with no
    // coverage diagnostics; regressions here mean the HIR lost ground.
    assert!(
        fully_covered * 2 >= sources.len(),
        "only {fully_covered}/{} examples fully lowered",
        sources.len()
    );
}

fn collect_wi_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_wi_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "wi") {
            out.push(path);
        }
    }
}
