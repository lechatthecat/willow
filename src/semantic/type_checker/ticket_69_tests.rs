//! Diagnostic recovery regressions for willow-jz15.69.
use super::*;
use crate::{lexer::Lexer, parser::Parser};

fn diagnostics(source: &str) -> Vec<Diagnostic> {
    let (mut program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    if !errors.is_empty() {
        return errors;
    }
    let mut imports = crate::import_phase(&program, std::path::Path::new("example"));
    let mut diagnostics = imports.outcome.diagnostics;
    let desugar = crate::desugar_phase(&mut program, &mut imports.graph.files);
    assert_eq!(desugar.error_count, 0, "{:?}", desugar.diagnostics);
    let artifacts = crate::module::artifacts::UnitArtifacts::new().unwrap();
    let checked = crate::typecheck_phase(
        &program,
        &imports.graph.files,
        &artifacts,
        &crate::CompilerOptions::debug(),
        None,
    )
    .unwrap();
    diagnostics.extend(checked.checker.errors.clone());
    diagnostics
}

#[test]
fn ticket_69_loop_hint_and_identifier_compatibility() {
    for source in [
        "fn main() { loop {} }",
        "fn main() { loop { break; } }",
        "fn main() { if true { loop {} } }",
        "fn main() { let x = loop {}; }",
    ] {
        let errors = diagnostics(source);
        assert!(
            errors.iter().any(|d| d.code == ErrorCode::E0102
                && d.message.contains("`loop { ... }` is not supported")
                && format!("{d:?}").contains("while true")),
            "{source}: {errors:?}"
        );
    }
    for source in [
        "fn main() { while true { break; } }",
        "fn main() { let loop = 1; println(loop); }",
        "fn main() { let loop = true; if loop {} }",
        "fn main() { let loop = true; while loop { break; } }",
        "fn main() { let loop = true; match loop { true => println(1), false => println(0) } }",
        "fn main() { let loop = [1]; for value in loop { println(value); } }",
        "fn main() { let loop = true; if true && loop {} }",
        "fn main() { let loop: Option<i64> = Some(1); if let Some(value) = loop { println(value); } }",
        "fn loop() -> i64 { return 1; } fn main() { println(loop()); }",
    ] {
        assert!(
            diagnostics(source).is_empty(),
            "{source}: {:?}",
            diagnostics(source)
        );
    }
}

#[test]
fn ticket_69_invalid_patterns_do_not_cascade() {
    for (scrutinee, arms) in [
        (
            "f64::parse(\"1\")",
            "Option::Some(x) => x, Option::None => 0.0",
        ),
        ("f64::parse(\"1\")", "Some(x) => x, None => 0.0"),
        (
            "f64::parse(\"1\")",
            "Option::Some(x) if x > 0.0 => x, Option::None => 0.0",
        ),
        (
            "f64::parse(\"1\")",
            "Option::Some(x) => match x { true => 1.0 }, _ => 0.0",
        ),
        ("true", "Option::Some(x) => x"),
        ("1", "Some(x) => x"),
        ("true", "1 => 2"),
        ("1", "true => 2"),
    ] {
        let source = format!("fn main() {{ let value = match {scrutinee} {{ {arms} }}; }}");
        let errors = diagnostics(&source);
        assert!(!errors.is_empty(), "{source}");
        assert!(
            errors.iter().all(|d| d.code == ErrorCode::E1205),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn ticket_69_independent_errors_and_valid_exhaustiveness_survive() {
    for (source, code) in [
        (
            "fn main() { match f64::parse(\"1\") { Some(x) => println(missing), _ => println(0) } }",
            ErrorCode::E0350,
        ),
        (
            "fn main() { match true { true => println(1) } }",
            ErrorCode::E1207,
        ),
        (
            "fn main() { match 1 { 1 => println(1) } }",
            ErrorCode::E1206,
        ),
        (
            "fn main() { match f64::parse(\"1\") { Ok(x) => println(x) } }",
            ErrorCode::E1202,
        ),
    ] {
        let errors = diagnostics(source);
        assert!(
            errors.iter().any(|d| d.code == code),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn ticket_69_collection_imports_and_unresolved_warnings() {
    for annotation in ["FrozenMap<i64, i64>", "FrozenArray<i64>"] {
        let name = annotation.split('<').next().unwrap();
        let source = format!(
            "import std::collections::{name}; fn take(value: {annotation}) {{}} fn main() {{}}"
        );
        assert!(
            diagnostics(&source).is_empty(),
            "{source}: {:?}",
            diagnostics(&source)
        );
    }
    for source in [
        "import std::collections::{FrozenArray, FrozenMap}; fn take(a: FrozenArray<i64>, m: FrozenMap<i64, i64>) {} fn main() {}",
        "import std::collections::FrozenMap as Snapshot; fn take(m: Snapshot<i64, i64>) {} fn main() {}",
    ] {
        assert!(
            diagnostics(source).is_empty(),
            "{source}: {:?}",
            diagnostics(source)
        );
    }
    for source in [
        "import std::collections::Missing; fn main() {}",
        "import std::collections::Missing as Nope; fn main() {}",
        "import std::missing; fn main() {}",
        "import std::collections::{FrozenMap, Missing}; fn take(m: FrozenMap<i64, i64>) {} fn main() {}",
    ] {
        let errors = diagnostics(source);
        assert!(
            errors.iter().any(|d| d.severity == Severity::Error),
            "{source}: {errors:?}"
        );
        assert!(
            !errors.iter().any(|d| d.code == ErrorCode::W2003),
            "{source}: {errors:?}"
        );
    }
    let errors = diagnostics("import std::collections::FrozenMap; fn main() {}");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, ErrorCode::W2003);
}

#[test]
fn ticket_69_recovery_visits_each_binding_once() {
    use super::check_lambda_match::INVALID_PATTERN_BINDINGS;
    for arms in [8, 32, 128] {
        let source = format!(
            "fn main() {{ match f64::parse(\"1\") {{ {} }} }}",
            "Option::Some(x) => println(x),".repeat(arms)
        );
        INVALID_PATTERN_BINDINGS.with(|count| count.set(0));
        let errors = diagnostics(&source);
        assert_eq!(errors.len(), arms, "{errors:?}");
        assert!(errors.iter().all(|d| d.code == ErrorCode::E1205));
        assert_eq!(INVALID_PATTERN_BINDINGS.with(|count| count.get()), arms);
    }
}
