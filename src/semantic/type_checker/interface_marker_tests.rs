use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;

fn check(source: &str) -> TypeChecker {
    let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}\n{source}");
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    checker
}

#[test]
fn interface_marker_contract_matrix() {
    // 2 markers x 6 field types x 4 layouts = 48 perspectives, each
    // checking acceptance/rejection and the offending source field label.
    for marker in ["Send", "Sync"] {
        for (ty, send, sync) in [
            ("i64", true, true),
            ("Plain", false, false),
            ("Array<i64>", true, false),
            ("Array<Plain>", false, false),
            ("FrozenArray<i64>", true, true),
            ("Mutex<Array<i64>>", true, true),
        ] {
            for layout in 0..4 {
                let field = format!("pub offending: {ty};");
                let declarations = match layout {
                    0 => format!("class Subject implements Contract {{ {field} }}"),
                    1 => format!(
                        "class Inner {{ {field} }} class Subject implements Contract {{ pub inner: Inner; }}"
                    ),
                    2 => format!(
                        "open class Base {{ {field} }} class Subject extends Base implements Contract {{}}"
                    ),
                    _ => format!(
                        "open class Base implements Contract {{}} class Subject extends Base {{ {field} }}"
                    ),
                };
                let source = format!(
                    "import std::collections::Array; interface Send {{}} interface Sync extends Send {{}} interface Plain {{}} interface Parent extends {marker} {{}} interface Contract extends Parent {{}} {declarations} fn main() {{}}"
                );
                let checker = check(&source);
                let accepted = if marker == "Send" { send } else { sync };
                assert_eq!(
                    checker.errors.len(),
                    usize::from(!accepted),
                    "{source}\n{:?}",
                    checker.errors
                );
                if !accepted {
                    let error = &checker.errors[0];
                    assert_eq!(error.code, ErrorCode::E2406);
                    let span = error.primary_span().unwrap();
                    assert!(
                        source[span.start..span.end].contains("offending"),
                        "{error:?}"
                    );
                    assert!(error.notes[0].contains("offending"), "{error:?}");
                }
            }
        }
    }
}

#[test]
fn interface_marker_contract_diamonds_scale() {
    for size in [16, 64, 256] {
        let mut source = String::from(
            "interface Send {} interface Root extends Send {} class C0 { pub value: i64; }",
        );
        for i in 1..=size {
            source.push_str(&format!(
                "class C{i} {{ pub left: C{}; pub right: C{}; }} interface I{i} extends Root {{}}",
                i - 1,
                i - 1
            ));
        }
        let contracts = (1..=size)
            .map(|i| format!("I{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        source.push_str(&format!(
            "class Subject implements {contracts} {{ pub graph: C{size}; }} fn main() {{}}"
        ));
        tests::REASON_VISITS.with(|n| n.set(0));
        let checker = check(&source);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let visits = tests::REASON_VISITS.with(|n| n.get());
        assert_eq!(visits, 2 * size + 3);
        eprintln!("interface-contract size={size} marker_visits={visits}");
    }
}

#[test]
fn interface_marker_contract_cycles_and_plain_interfaces() {
    for (fields, rejected) in [
        ("pub next: Subject;", false),
        ("pub next: Subject; pub bad: Plain;", true),
    ] {
        let source = format!(
            "interface Send {{}} interface Plain {{}} interface Contract extends Send {{}} class Subject implements Contract {{ {fields} }} fn main() {{}}"
        );
        let checker = check(&source);
        assert_eq!(
            checker.errors.len(),
            usize::from(rejected),
            "{:?}",
            checker.errors
        );
    }
    let checker =
        check("interface Plain {} class Subject implements Plain { pub bad: Plain; } fn main() {}");
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

#[test]
fn interface_marker_contract_repeated_roots_scale() {
    for size in [16, 64, 256] {
        let mut source = String::from(
            "interface Send {} interface Root extends Send {} class C0 { pub value: i64; }",
        );
        for i in 1..=size {
            source.push_str(&format!(
                "class C{i} {{ pub left: C{}; pub right: C{}; }}",
                i - 1,
                i - 1
            ));
        }
        for i in 0..size {
            source.push_str(&format!(
                "class Subject{i} implements Root {{ pub graph: C{size}; }}"
            ));
        }
        source.push_str("fn main() {}");
        tests::REASON_VISITS.with(|n| n.set(0));
        let checker = check(&source);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let visits = tests::REASON_VISITS.with(|n| n.get());
        assert_eq!(visits, 4 * size + 1);
        eprintln!("interface-contract roots={size} depth={size} marker_visits={visits}");
    }
}

#[test]
fn interface_marker_contract_rejected_cycles_do_not_publish_provisional_proofs() {
    let checker = check(
        "interface Send {} interface Plain {} interface Contract extends Send {} class A implements Contract { pub b: B; pub bad: Plain; } class B implements Contract { pub a: A; } fn main() {}",
    );
    assert_eq!(checker.errors.len(), 2, "{:?}", checker.errors);
    assert!(checker.errors.iter().all(|e| e.code == ErrorCode::E2406));
}

#[test]
fn interface_marker_contract_generic_payloads_and_static_fields() {
    for (ty, accepted) in [
        ("Payload<i64>", true),
        ("Payload<Plain>", false),
        ("Payload<Array<Plain>>", false),
    ] {
        let source = format!(
            "import std::collections::Array; interface Send {{}} interface Plain {{}} interface Contract extends Send {{}} enum Payload<T> {{ Value(T) }} class Subject implements Contract {{ pub data: {ty}; }} fn main() {{}}"
        );
        let checker = check(&source);
        assert_eq!(
            checker.errors.len(),
            usize::from(!accepted),
            "{:?}",
            checker.errors
        );
    }
    let checker = check(
        "interface Send {} interface Contract extends Send {} class Subject implements Contract { pub static callback: fn() -> i64 = value; } fn value() -> i64 { return 1; } fn main() {}",
    );
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}
