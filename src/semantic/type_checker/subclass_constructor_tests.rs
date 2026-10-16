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
            assert_eq!(errors.len(), 1, "{visibility}: {body}: {errors:?}");
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

#[test]
fn subclass_constructor_recovery_delegation_and_descendants() {
    for arg in ["", "1", "true", "1, 2", "&value"] {
        let errors = check(
            &format!(
                "open class Base {{ pub x: i64; pub init(self) {{ self.x = 1; }} }}
             open class Missing extends Base {{}}
             open class Middle extends Missing {{}}
             class Leaf extends Middle {{ pub init(self, value: i64) {{ super.init({arg}); }} }}
             fn main() {{ let value = 1; let a = new Missing({arg}); let b = new Middle({arg}); }}"
            ),
            false,
        );
        assert_eq!(missing(&errors), 1, "{errors:?}");
        assert_eq!(errors.len(), 1, "{arg}: {errors:?}");
    }
}

#[test]
fn subclass_constructor_recovery_keeps_independent_errors() {
    let errors = check(
        "open class Base { pub x: i64; pub init(self) { self.x = 1; } }
         open class Missing extends Base {}
         class Leaf extends Missing { pub init(self) { super.init(unknown_super); } }
         fn main() { let x = new Missing(unknown_new); let y: bool = x; }",
        false,
    );
    assert_eq!(missing(&errors), 1, "{errors:?}");
    for name in ["unknown_super", "unknown_new"] {
        assert!(
            errors
                .iter()
                .any(|d| d.code == ErrorCode::E0350 && d.message.contains(name)),
            "{errors:?}"
        );
    }
    assert!(
        errors.iter().any(|d| d.message.contains("expected `bool`")),
        "{errors:?}"
    );
    assert!(
        !errors
            .iter()
            .any(|d| d.code == ErrorCode::E0845 || d.message.contains("argument(s)")),
        "{errors:?}"
    );
}

thread_local! {
    pub(super) static FIELD_CHAIN_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[test]
fn subclass_constructor_recovery_counts_existing_field_walk() {
    for depth in [1, 8, 32] {
        for calls in [1, 8, 32] {
            let mut src = String::from("open class C0 { pub init(self) {} }");
            for i in 1..=depth {
                src.push_str(&format!("open class C{i} extends C{} {{}}", i - 1));
            }
            src.push_str("fn main() {");
            for i in 0..calls {
                src.push_str(&format!("let c{i} = new C{depth}();"));
            }
            src.push('}');
            FIELD_CHAIN_VISITS.with(|n| n.set(0));
            let errors = check(&src, false);
            assert_eq!(errors.len(), 1, "{errors:?}");
            let visits = FIELD_CHAIN_VISITS.with(|n| n.get());
            assert_eq!(visits, calls * (depth + 1));
            eprintln!(
                "constructor recovery depth={depth} calls={calls} field_walk_visits={visits}"
            );
        }
    }
}

#[test]
fn subclass_constructor_recovery_preserves_real_arity_errors() {
    for src in [
        "class C { pub x: i64; } fn main() { let c = new C(); }",
        "class C { pub init(self, x: i64) {} } fn main() { let c = new C(); }",
        "open class B { pub x: i64; } class C extends B { pub init(self) { super.init(); } } fn main() {}",
        "open class B { pub init(self, x: i64) {} } class C extends B { pub init(self) { super.init(); } } fn main() {}",
    ] {
        let errors = check(src, false);
        assert!(
            errors.iter().any(|d| d.message.contains("argument(s)")),
            "{errors:?}"
        );
        assert_eq!(missing(&errors), 0, "{errors:?}");
    }
}
