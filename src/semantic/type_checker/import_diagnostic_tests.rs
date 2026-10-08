use super::*;
use crate::{lexer::Lexer, parser::Parser};

fn diagnostics(source: &str) -> Vec<Diagnostic> {
    let (mut program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut imports = crate::import_phase(
        &program,
        std::path::Path::new("tests/fixtures/diagnostic_imports"),
    );
    assert_eq!(
        imports.outcome.error_count, 0,
        "{:?}",
        imports.outcome.diagnostics
    );
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
    checked.checker.errors.clone()
}

#[test]
fn ticket_42_collection_annotation_spans() {
    for (annotation, code) in [
        ("Array<i64>", ErrorCode::E2001),
        ("Map<i64, i64>", ErrorCode::E2002),
    ] {
        for source in [
            format!("fn main() {{ let data: {annotation} = 0; }}"),
            format!("fn main() {{ let mut data: {annotation} = 0; }}"),
            format!("fn take(data: {annotation}) {{}} fn main() {{}}"),
        ] {
            let errors = diagnostics(&source);
            let diagnostic = errors.iter().find(|d| d.code == code).unwrap();
            let span = diagnostic.primary_span().unwrap();
            assert_eq!(&source[span.start..span.end], annotation, "{source}");
        }
    }
}

#[test]
fn ticket_42_unused_imports() {
    for (source, unused) in [
        (
            "import helpers; fn main() { let text = helpers::read_to_string(\"x\"); }",
            None,
        ),
        (
            "import helpers as h; fn main() { let text = h::read_to_string(\"x\"); }",
            None,
        ),
        ("import helpers; fn main() {}", Some("helpers")),
        (
            "import helpers::Point; fn main() { let p = new Point(1); }",
            None,
        ),
        (
            "import helpers::Status; fn main() { let s = Status::Ready; }",
            None,
        ),
        (
            "import helpers::{Point, Status}; fn main() { let p = new Point(1); }",
            Some("Status"),
        ),
        (
            "import helpers::Point as T; enum Holder<T> { Value(T) } fn main() {}",
            Some("T"),
        ),
        ("import std::fs; fn main() {}", Some("fs")),
        ("import std::fs as numbers; fn main() {}", Some("numbers")),
        (
            "import std::collections::Array; fn main() {}",
            Some("Array"),
        ),
        (
            "import std::collections::Array as Items; fn main() {}",
            Some("Items"),
        ),
        (
            "import std::collections::Array; fn take(items: Array<i64>) {} fn main() {}",
            None,
        ),
        (
            "import std::collections::Array as Items; fn take(items: Items<i64>) {} fn main() {}",
            None,
        ),
        (
            "import std::collections; fn take(items: Array<i64>) {} fn main() {}",
            None,
        ),
        (
            "import std::fs; fn main() { let content = fs::read_to_string(\"file\"); }",
            None,
        ),
        (
            "import std::fs as numbers; fn main() { let content = numbers::read_to_string(\"file\"); }",
            None,
        ),
        (
            "import helpers::read_to_string; fn main() { let content = read_to_string(\"file\"); }",
            None,
        ),
        (
            "import helpers::read_to_string as root; fn main() { let content = root(\"file\"); }",
            None,
        ),
        (
            "import helpers::read_to_string as root; fn main() { let root = 3; println(root); }",
            Some("root"),
        ),
        (
            "import helpers::read_to_string as root; fn main() { let root = |x: f64| x; println(root(4.0)); }",
            Some("root"),
        ),
        ("import std::fs; fn main() { println(\"fs\"); }", Some("fs")),
        (
            "import std::fs; fn main() { /* fs::read_to_string */ }",
            Some("fs"),
        ),
        (
            "import std::fs; fn main() { let run = || fs::read_to_string(\"file\"); let content = run(); }",
            None,
        ),
        (
            "import std::collections::Array; import std::collections::Array as Items; fn take(items: Items<i64>) {} fn main() {}",
            Some("Array"),
        ),
    ] {
        let errors = diagnostics(source);
        assert!(
            !errors.iter().any(|d| d.severity == Severity::Error),
            "{source}: {errors:?}"
        );
        let warnings: Vec<_> = errors
            .iter()
            .filter(|d| d.code == ErrorCode::W2003)
            .collect();
        assert_eq!(
            warnings.len(),
            usize::from(unused.is_some()),
            "{source}: {errors:?}"
        );
        if let Some(name) = unused {
            assert_eq!(warnings[0].message, format!("unused import `{name}`"));
            assert_eq!(warnings[0].severity, Severity::Warning);
            let span = warnings[0].primary_span().unwrap();
            assert!(source[span.start..span.end].contains(name));
        }
    }
}

#[test]
fn ticket_42_frozen_help_uses_source_names() {
    for name in ["samples", "pixels", "entries"] {
        let source = format!(
            "fn visit({name}: FrozenArray<i64>) {{ for pixel in {name} {{}} }} fn main() {{}}"
        );
        let errors = diagnostics(&source);
        let error = errors
            .iter()
            .find(|d| d.message.contains("cannot iterate"))
            .unwrap();
        assert!(error.helps[0].contains(&format!("{name}.len()")));
        assert!(error.helps[0].contains("pixel"));
        assert!(!error.helps[0].contains("values"));
    }
}

thread_local! {
    pub(super) static IMPORT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[test]
fn ticket_42_import_probes_scale_linearly() {
    let mut previous = None;
    for count in [8, 16, 32, 64] {
        let source = format!(
            "import std::fs; {}",
            (0..count)
                .map(|i| format!("fn read{i}() {{ let result = fs::read_to_string(\"x\"); }}"))
                .collect::<String>()
        );
        let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(errors.is_empty());
        IMPORT_PROBES.with(|v| v.set(0));
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let probes = IMPORT_PROBES.with(|v| v.get());
        assert_eq!(checker.analysis_symbols.used_imports.len(), 1);
        if let Some(old) = previous {
            assert_eq!(probes, old * 2);
        }
        previous = Some(probes);
        eprintln!("import references: {count}, probes: {probes}, retained names: 1");
    }
}

// Shared with typed-body replay tests so both paths exercise exactly the same
// source-only alias uses (no annotation can accidentally count the alias).
pub(super) fn enum_alias_sources() -> Vec<String> {
    let mut sources = Vec::new();
    for value in ["res::Result::Ok(1)", "res::Result::Err(\"bad\")"] {
        for body in [
            format!("return {value};"),
            format!("let result: Result<i64, String> = {value}; return result;"),
            format!("return true ? {value} : Ok(0);"),
            format!("return match true {{ true => {value}, false => Ok(0) }};"),
            format!("let build: fn() -> Result<i64, String> = || {value}; return build();"),
        ] {
            sources.push(format!("import std::result as res; fn f() -> Result<i64, String> {{ {body} }} fn main() {{}}"));
        }
    }
    for alias in ["res", "outcomes"] {
        for body in [
            format!(
                "match r {{ {alias}::Result::Ok(n) => println(n), {alias}::Result::Err(e) => println(e) }}"
            ),
            format!("if let {alias}::Result::Ok(n) = r {{ println(n); }}"),
            format!(
                "let visit = || {{ match r {{ {alias}::Result::Ok(n) => println(n), {alias}::Result::Err(e) => println(e) }} }}; visit();"
            ),
        ] {
            sources.push(format!("import std::result as {alias}; fn f(r: Result<i64, String>) {{ {body} }} fn main() {{}}"));
        }
    }
    sources
}

#[test]
fn ticket_42_contextual_enum_and_pattern_imports() {
    for source in enum_alias_sources() {
        let errors = diagnostics(&source);
        assert!(errors.is_empty(), "{source}: {errors:?}");
    }
    // Canonicalizing a pattern must not falsely mark a second import as used.
    let source = "import std::result as res; import std::result as unused; fn f(r: Result<i64, String>) { match r { res::Result::Ok(n) => println(n), res::Result::Err(e) => println(e) } } fn main() {}";
    let errors = diagnostics(source);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "unused import `unused`");
    assert_eq!(errors[0].code, ErrorCode::W2003);
}

#[test]
fn ticket_42_enum_alias_import_probes_scale_linearly() {
    let mut previous = None;
    for count in [8, 16, 32, 64] {
        let functions: String = (0..count).map(|i| format!("fn f{i}(r: Result<i64, String>) -> Result<i64, String> {{ match r {{ res::Result::Ok(n) => println(n), res::Result::Err(e) => println(e) }} return res::Result::Ok(1); }}")).collect();
        let source = format!("import std::result as res; {functions}");
        let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        IMPORT_PROBES.with(|v| v.set(0));
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let probes = IMPORT_PROBES.with(|v| v.get());
        assert_eq!(checker.analysis_symbols.used_imports.len(), 1);
        if let Some(old) = previous {
            assert_eq!(probes, old * 2);
        }
        previous = Some(probes);
        eprintln!("enum alias functions: {count}, probes: {probes}, retained names: 1");
    }
}
