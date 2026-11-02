use willow_compiler::diagnostics::ErrorCode;
use willow_compiler::lexer::Lexer;
use willow_compiler::parser::Parser;

#[test]
fn select_non_block_bodies_explain_required_syntax() {
    // Seven arm forms crossed with five body forms cover binding and keyword paths.
    for arm in [
        "let v = ch.recv()",
        "ch.recv()",
        "let v = ch.recv_opt()",
        "ch.send(1)",
        "sleep(10)",
        "default",
        "let v = await task",
    ] {
        for body in [
            "println(v)",
            "consume(v)",
            "results.push(v)",
            "return",
            "x = v",
        ] {
            let source = format!("async fn main() {{ select {{ {arm} => {body}, }} }}");
            let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            let diagnostic = errors.first().expect("non-block body must fail");
            assert_eq!(diagnostic.code, ErrorCode::E0102, "{source}");
            assert_eq!(
                diagnostic.message, "select arm body must be a block",
                "{source}"
            );
            assert_eq!(
                diagnostic.primary_span().unwrap().start,
                source.find("=> ").unwrap() + 3
            );
            assert!(
                diagnostic
                    .helps
                    .iter()
                    .any(|help| help.contains("=> { println(v); }")),
                "{source}: {errors:?}"
            );
            assert!(
                errors
                    .iter()
                    .all(|error| !error.message.contains("reserved keyword")
                        && error.helps.iter().all(|help| !help.contains("rename"))),
                "{source}: {errors:?}"
            );

            let fixed = format!("async fn main() {{ select {{ {arm} => {{ {body}; }} }} }}");
            let (_, errors) = Parser::new(Lexer::new(&fixed).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{fixed}: {errors:?}");
        }
    }
}

#[test]
fn select_block_diagnostic_preserves_neighboring_errors() {
    for (source, expected) in [
        (
            "async fn main() { select { default println(1) } }",
            "unexpected token",
        ),
        (
            "async fn main() { select { default => {",
            "expected `}` to close block",
        ),
        ("fn println() {}", "reserved keyword"),
    ] {
        let (_, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(
            errors.iter().any(|error| error.message.contains(expected)),
            "{source}: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .all(|error| error.message != "select arm body must be a block")
        );
    }
}

#[test]
fn select_block_diagnostic_handles_missing_and_later_bodies() {
    for tail in ["", "}", ",", ";", "// body follows\n println(1)"] {
        let source = format!("async fn main() {{ select {{ default => {tail}");
        let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(
            errors
                .iter()
                .any(|error| error.message == "select arm body must be a block"),
            "{source}: {errors:?}"
        );
    }
    // Growing valid prefixes and expression tails must not affect localization.
    for count in [1, 16, 64, 256] {
        let prefix = "sleep(1) => {} ".repeat(count);
        let tail = "+ 1".repeat(count);
        let source =
            format!("async fn main() {{ select {{ {prefix}default => consume(0 {tail}); }} }}");
        let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert_eq!(errors.len(), 1, "count={count}: {errors:?}");
        assert_eq!(errors[0].message, "select arm body must be a block");
        assert_eq!(
            errors[0].primary_span().unwrap().start,
            source.find("consume").unwrap()
        );
    }
}
