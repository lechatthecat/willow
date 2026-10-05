use super::*;
use crate::lexer::Lexer;

const KEYWORDS: &str = "fn let mut if else while break continue defer for in return print println true false nil class pub prot open override static new extends interface implements self import module as async await select match enum const i64 f64 bool";

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
fn keyword_open_reserved_positions() {
    for source in [
        "fn open() {}",
        "class C { pub fn open(self) {} }",
        "fn f() { g.open(); }",
        "fn f() { let open = 1; }",
        "fn f() { let mut open = 1; }",
        "class C { pub open: i64; }",
        "class C { pub static open: i64 = 1; }",
        "interface I { fn open(self); }",
        "fn f(open: i64) {}",
        "fn f() { C::open(); }",
    ] {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (_, errors) = Parser::new(tokens).parse();
        let error = errors
            .iter()
            .find(|e| {
                e.code == ErrorCode::E0102 && e.message.contains("'open' is a reserved keyword")
            })
            .unwrap_or_else(|| panic!("{source}: {errors:?}"));
        assert!(error.helps.iter().any(|h| h.contains("rename")));
        assert_eq!(
            error.primary_span().unwrap().start,
            source.find("open").unwrap()
        );
    }
}

#[test]
fn keyword_valid_modifiers_and_nonkeywords_unchanged() {
    parse_ok("open class C {} interface I { fn f(self); }");
    parse_ok("fn opened() { let opening = 1; }");
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
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn keyword_diagnostic_token_reads_scale_linearly() {
    let mut samples = Vec::new();
    for n in [8, 16, 32, 64] {
        let source = format!("fn f() {{ {} }}", "let open = 1;".repeat(n));
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
