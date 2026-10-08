use super::*;

#[test]
fn diagnostic_recovery_method_uses() {
    for use_site in [
        "code.len();",
        "code.nope();",
        "let alias = code; alias.len();",
        "let n: i64 = code;",
        "take(code);",
        "let n = code + 1;",
        "let n = -code;",
        "let n = code[0];",
        "let n = code.field;",
        "let n = code.len().other();",
        "let n = [code];",
        "println(code);",
        "for item in code { item.len(); }",
        "if code {}",
        "while code {}",
        "let n = code ? 1 : 2;",
    ] {
        let source = format!(
            "import std::collections::Array; fn take(n: i64) {{}} fn main() {{ let input = [1, 2]; let code = input.clone(); {use_site} }}"
        );
        let errors = check_source(&source);
        assert_eq!(errors.len(), 1, "{use_site}: {errors:?}");
        assert!(errors[0].message.contains("clone"), "{errors:?}");
    }
}

#[test]
fn diagnostic_recovery_preserves_independent_errors() {
    for body in [
        "let n: i64 = true;",
        "let n = 1.nope();",
        "let n = absent;",
        "code.other(absent);",
        "let n = code[true];",
    ] {
        let errors = check_source(&format!(
            "import std::collections::Array; fn main() {{ let code = [1].clone(); {body} }}"
        ));
        assert!(errors.len() >= 2, "{body}: {errors:?}");
    }
}

#[test]
fn diagnostic_recovery_missing_type_compatibility() {
    for annotation in [
        "Missing",
        "Array<Missing>",
        "Result<i64, Missing>",
        "Option<Missing>",
    ] {
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        let tokens = crate::lexer::Lexer::new(&format!(
            "import std::collections::Array; fn take(x: {annotation}) {{}} fn main() {{}}"
        ))
        .tokenize()
        .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        checker.check_program(&program);
        assert!(
            checker
                .errors
                .iter()
                .any(|e| e.message.contains("cannot find type `Missing`")),
            "{:?}",
            checker.errors
        );
        assert!(checker.types_compatible(
            &Type::Generic(
                "Result".into(),
                vec![Type::I64, Type::Named("Missing".into())]
            ),
            &Type::Generic("Result".into(), vec![Type::I64, Type::String])
        ));
        assert!(!checker.types_compatible(
            &Type::Generic(
                "Result".into(),
                vec![Type::Bool, Type::Named("Missing".into())]
            ),
            &Type::Generic("Result".into(), vec![Type::I64, Type::String])
        ));
    }
}

#[test]
fn diagnostic_recovery_real_void_still_errors() {
    let errors = check_source("fn empty() {} fn main() { let value = empty(); value.len(); }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("void") && e.message.contains("no methods")),
        "{errors:?}"
    );
}

#[test]
fn diagnostic_recovery_import_visibility_and_shared_index() {
    let source = "pub enum Trap { Underflow } enum Secret { Hidden } pub class Machine {} pub interface Device { fn run(self); }";
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty());
    let mut checker = TypeChecker::new();
    checker.set_module_path("vm");
    checker.check_module_program(&program);
    assert!(
        checker
            .unknown_type_help("Trap")
            .contains("import vm::Trap;")
    );
    assert!(
        !checker
            .unknown_type_help("Secret")
            .contains("import vm::Secret")
    );
    assert!(std::rc::Rc::ptr_eq(
        &checker.import_type_hints,
        &checker.fork_body().import_type_hints
    ));
}

#[test]
fn diagnostic_recovery_fanout_counts() {
    for n in [16, 64, 256] {
        let mut source =
            "import std::collections::Array; fn main() { let code = [1].clone();".to_owned();
        for i in 0..n {
            source.push_str(&format!("let x{i} = code.len();"));
        }
        source.push('}');
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&program);
        assert_eq!(checker.errors.len(), 1);
        assert_eq!(checker.expr_types.len(), 2 * n + 3);
    }
}

#[test]
fn ticket_52_type_typo_spans_and_hints() {
    for (ty, expected) in [
        ("Arry", "Array"),
        ("FrozenArry<i64>", "FrozenArray"),
        ("Chanel<i64>", "Channel"),
        ("Strng", "String"),
    ] {
        for binding in ["let y", "let mut y"] {
            let source = format!("fn main() {{ {binding}: {ty} = 1; }}");
            let errors = check_source(&source);
            let error = errors
                .iter()
                .find(|d| d.message.starts_with("cannot find type"))
                .unwrap();
            assert!(
                error
                    .helps
                    .iter()
                    .any(|h| h.contains(&format!("did you mean `{expected}`"))),
                "{error:?}"
            );
            let span = error.labels[0].span;
            assert_eq!(&source[span.start..span.end], ty);
        }
    }
}

#[test]
fn ticket_52_channel_capacity_hint() {
    for call in [
        "Channel::new(1)",
        "Channel<i64>::new(1)",
        "Channel::new(1, 2)",
    ] {
        let errors = check_source(&format!("fn main() {{ let c = {call}; }}"));
        let error = errors
            .iter()
            .find(|d| d.message.contains("expects 0 arguments"))
            .unwrap();
        assert!(
            error
                .helps
                .iter()
                .any(|h| h.contains("Channel::with_capacity(n)"))
        );
    }
    for call in ["Channel<i64>::new()", "Channel<i64>::with_capacity(1)"] {
        assert!(check_source(&format!("fn main() {{ let c = {call}; }}")).is_empty());
    }
}

#[test]
fn ticket_52_typo_work_counts() {
    let checker = TypeChecker::new();
    // Built-in candidate lengths total 48; no scan over program declarations.
    for length in [8, 64, 512] {
        for repetitions in [1, 8, 64] {
            diagnostics::EDIT_CELLS.with(|cells| cells.set(0));
            for _ in 0..repetitions {
                checker.unknown_type_help(&"x".repeat(length));
            }
            let cells = diagnostics::EDIT_CELLS.with(|cells| cells.get());
            assert_eq!(cells, length * repetitions * 48);
            println!("length={length} repetitions={repetitions} cells={cells}");
        }
    }
}

#[test]
fn ticket_52_annotation_span_preserves_file_identity() {
    for ty in ["Arry", "FrozenArry<i64>", "Option<Arry>"] {
        let source = format!("fn main() {{ let value: {ty}=1; }}");
        let mut tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        for token in &mut tokens {
            token.span.file_id = crate::diagnostics::FileId(7);
        }
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        let error = checker
            .errors
            .iter()
            .find(|d| d.message.starts_with("cannot find type"))
            .unwrap();
        let span = error.labels[0].span;
        assert_eq!(span.file_id, crate::diagnostics::FileId(7));
        assert_eq!(&source[span.start..span.end], ty);
    }
}
