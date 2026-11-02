use super::*;
use crate::lexer::Lexer;

const KEYWORDS: &str = "fn let mut if else while break continue defer for in return print println true false nil class pub prot override static new extends interface implements self import module as async await select match enum const i64 f64 bool";

#[test]
fn keyword_identifier_diagnostics_all_spellings() {
    for keyword in KEYWORDS.split_whitespace() {
        let tokens = Lexer::new(keyword).tokenize().unwrap();
        assert_eq!(tokens[0].kind.keyword_name(), Some(keyword));
        let mut parser = Parser::new(tokens);
        if keyword == "new" {
            assert_eq!(parser.expect_ident().unwrap(), "new");
            continue;
        }
        let error = parser.expect_ident().unwrap_err();
        assert_eq!(error.code, ErrorCode::E0102);
        assert!(
            error
                .message
                .contains(&format!("'{keyword}' is a reserved keyword"))
        );
        assert!(error.helps.iter().any(|h| h.contains("rename")));
        assert_eq!(error.primary_span().unwrap().start, 0);
    }
}

#[test]
fn keyword_open_contextual_positions() {
    // Distinct name/expression positions, including branches whose lookahead
    // used to classify reserved keywords differently from identifiers.
    for source in [
        "fn open() {}",
        "class C { pub fn open(self) {} }",
        "fn f() { g.open(); }",
        "fn f() { let open = 1; println(open); }",
        "fn f() { let mut open = 1; open += 1; }",
        "class C { pub open: i64; }",
        "class C { pub static open: i64 = 1; }",
        "interface I { fn open(self); }",
        "fn f(open: i64) { println(open); }",
        "fn f() { C::open(); }",
        "fn f() { let open = true; println(true ? open : false); }",
        "fn f() { let open = 1; println(open + open); }",
        "fn f() { let open = 1; g(open); }",
        "fn f() { let open = 1; return open; }",
        "fn f() { let open = true; if open { println(1); } }",
        "fn f() { let open = false; while open {} }",
        "fn f() { let open = [1]; println(open[0]); }",
        "fn f() { for open in [1] { println(open); } }",
        "fn f() { let open = 1; let g = || open; }",
        "fn f() { let g = |open: i64| open; }",
        "class C { pub static mut open: i64 = 1; }",
        "open class C { pub open fn open(self) {} }",
        "open class C { pub open async fn open(self) {} }",
        "fn f() { let open = 1; if true { let open = 2; println(open); } println(open); }",
    ] {
        parse_ok(source);
    }
    let tokens = Lexer::new("open").tokenize().unwrap();
    assert_eq!(tokens[0].kind, TokenKind::Ident("open".into()));
}

#[test]
fn keyword_valid_modifiers_and_nonkeywords_unchanged() {
    parse_ok("open class C {} interface I { fn f(self); }");
    parse_ok("fn opened() { let opening = 1; }");
    parse_ok("import std::io::{print, println}; fn main() { print(1); println(2); }");
    for source in ["fn 123() {}", "fn () {}"] {
        let errors = parse_errors(source);
        assert!(errors.iter().any(|e| e.code == ErrorCode::E0102));
        assert!(
            errors
                .iter()
                .all(|e| !e.message.contains("reserved keyword"))
        );
    }
}

fn parse_errors(source: &str) -> Vec<Diagnostic> {
    let tokens = Lexer::new(source).tokenize().unwrap();
    Parser::new(tokens).parse().1
}

fn parse_ok(source: &str) {
    let errors = parse_errors(source);
    assert!(errors.is_empty(), "{source}: {errors:?}");
}

#[test]
fn keyword_diagnostic_token_reads_scale_linearly() {
    let mut samples = Vec::new();
    for n in [8, 16, 32, 64] {
        let source = format!("fn f() {{ {} }}", "let override = 1;".repeat(n));
        PARSER_TOKEN_READS.with(|count| count.set(0));
        let errors = parse_errors(&source);
        assert_eq!(errors.len(), n);
        samples.push((n, PARSER_TOKEN_READS.with(|count| count.get())));
    }
    let per_error = (samples[1].1 - samples[0].1) / (samples[1].0 - samples[0].0);
    for pair in samples.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, per_error * (pair[1].0 - pair[0].0));
    }
    eprintln!("keyword diagnostic token reads: {samples:?}");
}

#[test]
fn keyword_diagnostics_in_declarations_members_locals_and_fields() {
    for keyword in KEYWORDS.split_whitespace().filter(|word| *word != "new") {
        for source in [
            format!("fn {keyword}() {{}}"),
            format!("fn f() {{ g.{keyword}(); }}"),
            format!("fn f() {{ let mut {keyword} = 1; }}"),
            format!("class C {{ pub static mut {keyword}: i64 = 1; }}"),
        ] {
            let errors = parse_errors(&source);
            assert!(
                errors.iter().any(|e| e.code == ErrorCode::E0102
                    && e.message
                        .contains(&format!("'{keyword}' is a reserved keyword"))
                    && e.helps.iter().any(|h| h.contains("rename"))),
                "{source}: {errors:?}"
            );
        }
    }
}

#[test]
fn builtin_method_names_recover_without_receiver_cascades() {
    for keyword in ["print", "println"] {
        for source in [
            format!(
                "interface Printer {{ fn {keyword}(self, v: Json) -> String; fn name(self) -> String; }} fn after() {{}}"
            ),
            format!(
                "interface Printer {{ fn {keyword}(self) {{ if true {{ println(1); }} }} fn name(self); }} fn after() {{}}"
            ),
            format!(
                "class Printer {{ pub fn {keyword}(self) {{}} pub fn name(self) {{}} }} fn after() {{}}"
            ),
            format!("fn f() {{ p.{keyword}(v); }} fn after() {{}}"),
            format!("fn f() {{ new X().{keyword}(v); }} fn after() {{}}"),
        ] {
            let tokens = Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = Parser::new(tokens).parse();
            assert_eq!(errors.len(), 1, "{source}: {errors:?}");
            assert!(errors[0].message.contains("builtin function name"));
            assert_eq!(
                errors[0].primary_span().unwrap().start,
                source.find(keyword).unwrap()
            );
            assert!(
                program
                    .items
                    .iter()
                    .any(|item| matches!(item, ast::Item::Function(f) if f.name == "after"))
            );
            for item in &program.items {
                match item {
                    ast::Item::Interface(i) => assert_eq!(i.methods[1].name, "name"),
                    ast::Item::Class(c) => assert_eq!(c.methods[1].name, "name"),
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn builtin_method_recovery_token_reads_scale_linearly() {
    let mut samples = Vec::new();
    for n in [8, 16, 32, 64] {
        let source = format!(
            "interface I {{ {} fn name(self); }}",
            "fn print(self);".repeat(n)
        );
        PARSER_TOKEN_READS.with(|count| count.set(0));
        assert_eq!(parse_errors(&source).len(), n);
        samples.push((n, PARSER_TOKEN_READS.with(|count| count.get())));
    }
    let slope = (samples[1].1 - samples[0].1) / 8;
    for pair in samples.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
    }
    eprintln!("builtin recovery token reads: {samples:?}");
}

#[test]
fn keyword_open_token_reads_scale_linearly() {
    let mut samples = Vec::new();
    for n in [8, 16, 32, 64] {
        let source = format!("fn f() {{ {} }}", "let open = 1; println(open);".repeat(n));
        PARSER_TOKEN_READS.with(|count| count.set(0));
        parse_ok(&source);
        samples.push((n, PARSER_TOKEN_READS.with(|count| count.get())));
    }
    let slope = (samples[1].1 - samples[0].1) / 8;
    for pair in samples.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
    }
    eprintln!("contextual open token reads: {samples:?}");
}

#[test]
fn keyword_open_preserves_modifiers_and_recovery() {
    let source = "let broken = ; open class C { pub open: i64; pub open fn open(self) {} }";
    let tokens = Lexer::new(source).tokenize().unwrap();
    let (program, errors) = Parser::new(tokens).parse();
    assert!(!errors.is_empty());
    let ast::Item::Class(class) = program.items.last().unwrap() else {
        panic!("missing class")
    };
    assert!(class.is_open);
    assert_eq!(class.fields[0].name, "open");
    assert_eq!(class.methods[0].name, "open");
    assert!(class.methods[0].is_open);
}
