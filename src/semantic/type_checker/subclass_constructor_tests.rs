//! Regression perspectives: public/protected/private base init, each with
//! unused, zero-argument, matching-field, mismatching-field, extra-argument,
//! and repeated construction (18); forward declaration, module scope, deep
//! chain, fan-out, memberwise preservation, explicit delegation, cycle (7).
use crate::diagnostics::{Diagnostic, ErrorCode};

fn check(src: &str, module: bool) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    if module {
        checker.set_module_path("m");
        checker.check_module_program(&program);
    } else {
        checker.check_program(&program);
    }
    checker.errors
}

fn missing(errors: &[Diagnostic]) -> usize {
    errors
        .iter()
        .filter(|d| d.code == ErrorCode::E0848 && d.message.contains("must declare `init`"))
        .count()
}

#[test]
fn subclass_constructor_visibility_and_arguments() {
    for visibility in ["pub", "prot", ""] {
        for body in [
            "",
            "let c = new Child();",
            "let c = new Child(1);",
            "let c = new Child(true);",
            "let c = new Child(1, 2);",
            "let a = new Child(1); let b = new Child(2);",
        ] {
            let src = format!(
                "open class Base {{ pub x: i64; {visibility} init(self) {{ self.x = 42; }} }}
                class Child extends Base {{}} fn main() {{ {body} }}"
            );
            let errors = check(&src, false);
            assert_eq!(missing(&errors), 1, "{visibility}: {body}: {errors:?}");
            let d = errors
                .iter()
                .find(|d| d.message.contains("must declare `init`"))
                .unwrap();
            assert!(
                d.helps
                    .iter()
                    .any(|h| h.contains("init(self, ...)") && h.contains("super.init(...)"))
            );
            assert_eq!(d.labels.len(), 2);
        }
    }
}

#[test]
fn subclass_constructor_forward_and_module() {
    for module in [false, true] {
        let errors = check(
            "class Child extends Base {} open class Base { pub init(self) {} } fn main() {}",
            module,
        );
        assert_eq!(missing(&errors), 1, "{errors:?}");
    }
}

#[test]
fn subclass_constructor_scaling_shapes() {
    // Exact diagnostic counts, independent of timings. Only the first edge
    // in a chain is invalid; every immediate child in a fan-out is invalid.
    for n in [8, 32, 128] {
        for chain in [false, true] {
            let mut src = String::from("open class C0 { pub init(self) {} } ");
            for i in 1..=n {
                let base = if chain { i - 1 } else { 0 };
                src.push_str(&format!("open class C{i} extends C{base} {{}} "));
            }
            src.push_str("fn main() {}");
            let errors = check(&src, false);
            assert_eq!(missing(&errors), if chain { 1 } else { n }, "{errors:?}");
        }
    }
}

#[test]
fn subclass_constructor_preserves_valid_memberwise_and_delegation() {
    for src in [
        "open class Base { pub x: i64; } class Child extends Base {} fn main() { let c = new Child(1); }",
        "open class Base { pub init(self) {} } class Child extends Base { pub init(self) { super.init(); } } fn main() { let c = new Child(); }",
    ] {
        let errors = check(src, false);
        assert!(errors.is_empty(), "{errors:?}");
    }
}

#[test]
fn subclass_constructor_cycle_keeps_primary_diagnostic() {
    let errors = check(
        "open class Base extends Child { pub init(self) {} } open class Child extends Base {} fn main() {}",
        false,
    );
    assert_eq!(missing(&errors), 0, "{errors:?}");
    assert!(errors.iter().any(|d| d.code == ErrorCode::E0426));
}
