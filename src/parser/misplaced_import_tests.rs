use super::*;

#[test]
fn misplaced_import_linear_recovery() {
    let mut counts = Vec::new();
    for n in [8, 16, 32, 64] {
        let src = format!(
            "fn main() {{}} {} class After {{}}",
            "import std::io::{print, println}; ".repeat(n)
        );
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        PARSER_TOKEN_READS.with(|c| c.set(0));
        let (program, errors) = Parser::new(tokens).parse();
        let reads = PARSER_TOKEN_READS.with(|c| c.get());
        assert_eq!(errors.len(), n);
        assert_eq!(program.items.len(), 2);
        eprintln!("misplaced imports={n} token_reads={reads}");
        counts.push(reads);
    }
    for triple in counts.windows(3) {
        assert_eq!(triple[2] - triple[1], 2 * (triple[1] - triple[0]));
    }
}

#[test]
fn misplaced_import_recovery_and_valid_prefix() {
    for prefix in [
        "",
        "module app;",
        "import std::io::println;",
        "module app; import std::io::println;",
    ] {
        let src = format!(
            "{prefix} fn main() {{}} import std::io::print; import std::io::println; class C {{}}"
        );
        let (program, errors) =
            Parser::new(crate::lexer::Lexer::new(&src).tokenize().unwrap()).parse();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors
                .iter()
                .all(|d| d.message.contains("imports must appear before"))
        );
        assert_eq!(program.items.len(), 2);
        let valid = format!("{prefix} fn main() {{}}");
        let (_, errors) = Parser::new(crate::lexer::Lexer::new(&valid).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
    }
}

#[test]
fn misplaced_import_after_each_item() {
    for item in [
        "fn before() {}",
        "class C {}",
        "open class C {}",
        "enum E { A }",
        "interface I {}",
        "const X: i64 = 1;",
        "pub fn before() {}",
        "async fn before() {}",
    ] {
        for import in [
            "import std::io::println;",
            "import std::io::{print, println};",
            "import std::io::println as output;",
        ] {
            let source = format!("{item} {import} fn main() {{}}");
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = Parser::new(tokens).parse();
            assert_eq!(errors.len(), 1, "{source}: {errors:?}");
            assert_eq!(errors[0].code, ErrorCode::E0105);
            assert!(
                errors[0].message.contains("imports must appear before"),
                "{errors:?}"
            );
            assert_eq!(errors[0].helps.len(), 1);
            assert_eq!(program.items.len(), 2, "recovery lost the following item");
        }
    }
}
