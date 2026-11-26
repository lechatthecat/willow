//! Hints for common slips: a missing `;` points at the end of the statement,
//! a stray `;` is named as such, an unknown function suggests the nearest
//! visible one, a match-arm assignment shows the wrapped statement, and a
//! map's `set`/`put` suggests `insert`.
use crate::diagnostics::source_map::SourceMap;
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

fn parse_errors(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    crate::parser::Parser::new(tokens).parse().1
}

fn check_errors(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

fn apply_fixes(source: &str, diagnostic: &Diagnostic) -> String {
    let mut fixes: Vec<_> = diagnostic.fix_suggestions.iter().collect();
    fixes.sort_by_key(|f| std::cmp::Reverse((f.span.start, f.span.end)));
    let mut fixed = source.to_string();
    for fix in fixes {
        fixed.replace_range(fix.span.start..fix.span.end, &fix.replacement);
    }
    fixed
}

fn render(source: &str, diagnostic: &Diagnostic) -> String {
    let map = SourceMap::new("t.wi", source);
    let mut out = Vec::new();
    crate::diagnostics::reporter::emit_with(diagnostic, &map, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn missing_semicolon(source: &str) -> Diagnostic {
    let errors = parse_errors(source);
    errors
        .into_iter()
        .find(|d| d.message == "expected `;` after statement")
        .unwrap_or_else(|| panic!("{source}"))
}

#[test]
fn missing_semicolon_points_after_the_previous_line() {
    for (name, source, after, line) in [
        (
            "let",
            "fn main() {\n    let x = 1\n    let y = 2;\n}",
            "1",
            2,
        ),
        ("call", "fn main() {\n    f()\n    g();\n}", "f()", 2),
        (
            "before closing brace",
            "fn main() {\n    let x = 1\n}",
            "1",
            2,
        ),
        (
            "trailing comment",
            "fn main() {\n    let x = 1 // one\n    let y = 2;\n}",
            "1",
            2,
        ),
        (
            "blank lines between",
            "fn main() {\n    let x = 1\n\n\n    let y = 2;\n}",
            "1",
            2,
        ),
        (
            "crlf",
            "fn main() {\r\n    let x = 1\r\n    let y = 2;\r\n}",
            "1",
            2,
        ),
        (
            "return",
            "fn f() -> i64 {\n    return 4\n}\nfn main() { f(); }",
            "return 4",
            2,
        ),
    ] {
        let error = missing_semicolon(source);
        let span = error.labels[0].span;
        let end = source.find(after).expect("anchor") + after.len();
        assert_eq!(
            (span.start, span.end, span.line),
            (end, end, line),
            "{name}"
        );
        let line_start = source[..end].rfind('\n').map_or(0, |i| i + 1);
        assert_eq!(span.col, end - line_start + 1, "{name}");
        assert_eq!(error.labels[0].message, "add `;` here", "{name}");
        assert!(
            error
                .helps
                .iter()
                .any(|h| h.contains(&format!("line {line}"))),
            "{name}: {:?}",
            error.helps
        );
        let fixed = apply_fixes(source, &error);
        let remaining = parse_errors(&fixed);
        assert!(remaining.is_empty(), "{name}: {fixed}: {remaining:?}");
    }
}

#[test]
fn missing_semicolon_on_one_line_keeps_the_next_token_position() {
    let source = "fn main() { let x = 1 let y = 2; }";
    let error = missing_semicolon(source);
    let span = error.labels[0].span;
    assert_eq!(span.start, source.find("let y").unwrap(), "{error:?}");
    assert!(error.fix_suggestions.is_empty(), "{error:?}");
}

#[test]
fn missing_semicolon_after_multiline_string_keeps_the_next_token_position() {
    let source = "fn main() {\n    let s = \"a\nb\"\n    let y = 2;\n}";
    let error = missing_semicolon(source);
    assert_eq!(
        error.labels[0].span.start,
        source.find("let y").unwrap(),
        "{error:?}"
    );
    assert!(error.fix_suggestions.is_empty(), "{error:?}");
}

#[test]
fn missing_semicolon_renders_the_fixed_line() {
    let source = "fn main() {\n    let x = 1\n    let y = 2;\n}";
    let rendered = render(source, &missing_semicolon(source));
    assert!(rendered.contains(" --> t.wi:2:14"), "{rendered}");
    assert!(rendered.contains("2 |     let x = 1;\n"), "{rendered}");
    assert!(rendered.contains("  |              +\n"), "{rendered}");
}

#[test]
fn stray_semicolons_report_one_error_per_run() {
    for (name, source, runs) in [
        ("double", "fn main() { f();; }", vec![";"]),
        ("long run", "fn main() { f();;;; }", vec![";;;"]),
        (
            "after block statement",
            "fn main() { if true { f(); }; }",
            vec![";"],
        ),
        ("block start", "fn main() { ; f(); }", vec![";"]),
        ("two runs", "fn main() { f();; g();;; }", vec![";", ";;"]),
        (
            "nested block",
            "fn main() { while false { f();; } }",
            vec![";"],
        ),
        (
            "closure statement",
            "fn main() { let c = || 1;; }",
            vec![";"],
        ),
    ] {
        let errors = parse_errors(source);
        assert_eq!(errors.len(), runs.len(), "{name}: {errors:?}");
        let mut search = 0;
        for (error, run) in errors.iter().zip(&runs) {
            assert_eq!(error.code, ErrorCode::E0102, "{name}");
            assert_eq!(error.message, "unnecessary `;`", "{name}");
            let span = error.labels[0].span;
            assert_eq!(&source[span.start..span.end], *run, "{name}");
            assert!(span.start >= search, "{name}: source order");
            search = span.end;
        }
        let mut fixed = source.to_string();
        for error in errors.iter().rev() {
            fixed = apply_fixes(&fixed, error);
        }
        assert!(parse_errors(&fixed).is_empty(), "{name}: {fixed}");
    }
}

#[test]
fn stray_semicolon_does_not_hide_later_errors() {
    let errors = parse_errors("fn main() { f();; let x = ; }");
    let messages: Vec<_> = errors.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(messages.len(), 2, "{errors:?}");
    assert_eq!(messages[0], "unnecessary `;`", "{errors:?}");
}

#[test]
fn statement_terminators_stay_accepted() {
    for source in [
        "fn main() { match 1 { _ => {} }; }",
        "fn main() { f(); g(); }",
        "fn main() { }",
        "fn main() { if true { f(); } }",
    ] {
        let errors = parse_errors(source);
        assert!(errors.is_empty(), "{source}: {errors:?}");
    }
}

fn unknown_function_help(source: &str, callee: &str) -> Option<String> {
    let errors = check_errors(source);
    let error = errors
        .iter()
        .find(|d| d.message == format!("cannot find function `{callee}`"))
        .unwrap_or_else(|| panic!("{source}: {errors:?}"));
    error.helps.first().cloned()
}

#[test]
fn unknown_function_suggests_the_nearest_visible_function() {
    for (name, source, callee, suggestion) in [
        (
            "leading word of a builtin",
            "async fn main() { await yield_now(); }",
            "yield_now",
            Some("yield"),
        ),
        (
            "typo of a user function",
            "fn compute(x: i64) -> i64 { return x; }\nfn main() { println(compte(1)); }",
            "compte",
            Some("compute"),
        ),
        (
            "unit suffix",
            "fn main() { sleep_ms(3); }",
            "sleep_ms",
            Some("sleep"),
        ),
        (
            "print keyword",
            "fn main() { printn(\"a\"); }",
            "printn",
            Some("print"),
        ),
        (
            "no similar name",
            "fn main() { zzqqxx(1); }",
            "zzqqxx",
            None,
        ),
        (
            "methods are not free functions",
            "class C { pub fn compute(self) -> i64 { return 1; } }\nfn main() { println(compte()); }",
            "compte",
            None,
        ),
    ] {
        let help = unknown_function_help(source, callee);
        assert_eq!(
            help,
            suggestion.map(|s| format!("did you mean `{s}`?")),
            "{name}"
        );
    }
}

fn match_arm_assignment(source: &str) -> Diagnostic {
    parse_errors(source)
        .into_iter()
        .find(|d| d.message == "assignment in a match arm requires a block")
        .unwrap_or_else(|| panic!("{source}"))
}

#[test]
fn match_arm_assignment_renders_the_wrapped_statement() {
    let source = "fn main() {\n    match 1 { 1 => o += 1, _ => {} }\n}";
    let error = match_arm_assignment(source);
    assert_eq!(
        apply_fixes(source, &error),
        "fn main() {\n    match 1 { 1 => { o += 1; }, _ => {} }\n}"
    );
    let rendered = render(source, &error);
    assert!(
        rendered.contains("2 |     match 1 { 1 => { o += 1; }, _ => {} }\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("  |                    ++      +++\n"),
        "{rendered}"
    );
    // One fixed line for both insertions.
    assert_eq!(rendered.matches("2 |").count(), 2, "{rendered}");
}

#[test]
fn match_arm_assignment_with_a_bad_value_keeps_the_generic_help() {
    let error = match_arm_assignment("fn main() { match 1 { 1 => o = , _ => {} } }");
    assert!(error.fix_suggestions.is_empty(), "{error:?}");
    assert!(
        error
            .helps
            .iter()
            .any(|h| h.contains("{ target = value; }")),
        "{error:?}"
    );
}

#[test]
fn fixes_on_different_lines_render_separately() {
    use crate::diagnostics::label::FixSuggestion;
    use crate::diagnostics::{Label, Span};
    let source = "ab\ncd\n";
    let error = Diagnostic::new(Severity::Error, ErrorCode::E0102, "x")
        .with_label(Label::primary(Span::new(0, 1, 1, 1), "here"))
        .with_fix(FixSuggestion::insertion(Span::new(2, 2, 1, 3), ";", "a"))
        .with_fix(FixSuggestion::new(Span::new(3, 4, 2, 1), "", "b"));
    let rendered = render(source, &error);
    assert!(rendered.contains("1 | ab;\n  |   +\n"), "{rendered}");
    assert!(rendered.contains("2 | d\n  | ~\n"), "{rendered}");
}

fn map_method_helps(method: &str) -> Vec<String> {
    let source = format!(
        "import std::collections::Map;\nfn main() {{ let m: Map<String, i64> = Map::new(); m.{method}(\"a\", 1); }}"
    );
    let errors = check_errors(&source);
    assert_eq!(errors.len(), 1, "{method}: {errors:?}");
    assert_eq!(errors[0].code, ErrorCode::E0201, "{method}");
    errors[0].helps.clone()
}

#[test]
fn map_setter_names_suggest_insert() {
    for method in ["set", "put", "add", "update"] {
        let helps = map_method_helps(method);
        assert_eq!(
            helps[0],
            format!("use `.insert(k, v)` to set a value; there is no `.{method}`"),
            "{method}"
        );
        assert!(helps[1].starts_with("maps support"), "{method}: {helps:?}");
    }
    let helps = map_method_helps("frob");
    assert_eq!(helps.len(), 1, "{helps:?}");
    assert!(helps[0].starts_with("maps support"), "{helps:?}");
}

#[test]
fn map_insert_is_accepted() {
    let errors = check_errors(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); m.insert(\"a\", 1); println(m.len()); }",
    );
    assert!(errors.is_empty(), "{errors:?}");
}
