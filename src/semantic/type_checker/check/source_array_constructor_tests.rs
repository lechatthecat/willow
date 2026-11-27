use super::*;

#[test]
fn array_utilities_nested_constructor_checks_are_linear() {
    for depth in [4, 8, 16] {
        let mut value = "7".to_string();
        for _ in 0..depth {
            value = format!("Array::with_len(1, {value})");
        }
        ARRAY_CONSTRUCTOR_ARGUMENT_CHECKS.with(|count| count.set(0));
        let errors = check_source(&format!(
            "import std::collections::Array; fn main() {{ let a = {value}; }}"
        ));
        assert!(errors.is_empty(), "{errors:?}");
        let checks = ARRAY_CONSTRUCTOR_ARGUMENT_CHECKS.with(|count| count.get());
        eprintln!("array constructor depth={depth} inferred argument checks={checks}");
        assert_eq!(checks, depth);
    }
}

#[test]
fn source_array_unresolved_constructor_checks_declared_arguments() {
    assert!(
        check_source("import std::collections::Array; fn main() { println(Array::new().len()); }")
            .is_empty()
    );
    let errors =
        check_source("import std::collections::Array; fn main() { println(Array::new(1).len()); }");
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("argument")),
        "{errors:?}"
    );
}

#[test]
fn source_array_constructor_signature_is_authoritative() {
    for (parameters, succeeds) in [("", true), ("value: i64", false)] {
        let source = format!(
            "pub class Array<T> {{ pub static fn new({parameters}) -> i64 {{ return 73; }} }}"
        );
        let tokens =
            crate::lexer::Lexer::with_file_id(&source, crate::diagnostics::FileId(u32::MAX))
                .tokenize()
                .unwrap();
        let (mut bundled, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let Item::Class(class) = &mut bundled.items[0] else {
            unreachable!()
        };
        class.abi_storage = Some(Type::Array(Box::new(Type::Named("T".into()))));
        let mut checker = TypeChecker::new();
        checker.register_module_type_signatures(
            "$std::collections",
            "<std>/collections.wi",
            &bundled,
        );
        let tokens = crate::lexer::Lexer::new(
                "import std::collections::Array; fn main() { let value: i64 = Array<i64>::new(); let inferred: i64 = Array::new(); }",
            )
            .tokenize()
            .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        checker.check_program(&program);
        assert_eq!(checker.errors.is_empty(), succeeds, "{:?}", checker.errors);
        if succeeds {
            assert!(
                checker
                    .static_call_classes
                    .values()
                    .any(|owner| owner.to_string().starts_with("$std::collections::"))
            );
        } else {
            assert!(
                checker
                    .errors
                    .iter()
                    .any(|error| error.message.contains("argument")),
                "{:?}",
                checker.errors
            );
        }
    }
}
