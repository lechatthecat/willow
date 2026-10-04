//! `super.method(args)` checking and unresolved-name recovery (willow-jz15.48).
//!
//! Perspectives:
//!   1. a super call from an override checks clean and takes the base's type
//!   2. `super` in a free function is E0851
//!   3. `super` in a class without a base class is E0851
//!   4. `super` in a static method is E0851
//!   5. a bare `super` value is E0851
//!   6. `super.field` is E0851: fields are read through `self`
//!   7. a missing base method is a single E0502
//!   8. a private base method is E0501
//!   9. `super` inside a lambda is a `self` capture (E1002)
//!  10. an argument mismatch on a super call is a single E0201
//!  11. a super call's result type is the base's return type
//!  12. a local named `super` is an ordinary value
//!  13. an unresolved name reports once per use and never cascades
//!  14. an unresolved name in typed contexts produces no follow-on mismatch
use super::{ErrorCode, check_source};

const BASE: &str = r#"
open class A {
    pub n: i64;
    pub open fn value(self, k: i64) -> i64 { return self.n + k; }
    fn hidden(self) -> i64 { return 0; }
}
"#;

#[track_caller]
fn codes(source: &str) -> Vec<ErrorCode> {
    check_source(source).into_iter().map(|e| e.code).collect()
}

#[track_caller]
fn assert_codes(source: &str, expected: &[ErrorCode]) {
    let errors = check_source(source);
    let actual: Vec<_> = errors.iter().map(|e| e.code).collect();
    assert_eq!(actual, expected, "{errors:?}");
}

#[test]
fn super_call_from_an_override_checks_clean() {
    assert_codes(
        &format!(
            "{BASE} class B extends A {{ pub override fn value(self, k: i64) -> i64 {{ return super.value(k) + 1; }} }} fn main() {{}}"
        ),
        &[],
    );
}

#[test]
fn super_outside_an_instance_method_is_rejected() {
    // A free function, a class without a base, and a static method.
    for source in [
        "fn main() { super.value(1); }".to_string(),
        "class Solo { pub fn f(self) -> i64 { return super.f(); } } fn main() {}".to_string(),
        format!(
            "{BASE} class B extends A {{ pub static fn make() -> i64 {{ return super.value(1); }} }} fn main() {{}}"
        ),
    ] {
        assert_codes(&source, &[ErrorCode::E0851]);
    }
}

#[test]
fn super_is_not_a_value() {
    for body in ["let s = super;", "let n = super.n;"] {
        assert_codes(
            &format!("{BASE} class B extends A {{ pub fn f(self) {{ {body} }} }} fn main() {{}}"),
            &[ErrorCode::E0851],
        );
    }
}

#[test]
fn super_method_lookup_errors_do_not_cascade() {
    for (body, code) in [
        (
            "let x: i64 = super.nope(); let y = x + 1;",
            ErrorCode::E0502,
        ),
        ("let x = super.hidden();", ErrorCode::E0501),
        ("let x = super.value(true);", ErrorCode::E0201),
        ("let x: String = super.value(1);", ErrorCode::E0201),
    ] {
        let found = codes(&format!(
            "{BASE} class B extends A {{ pub fn f(self) {{ {body} }} }} fn main() {{}}"
        ));
        assert_eq!(found, vec![code], "{body}");
    }
}

#[test]
fn super_inside_a_lambda_is_a_self_capture() {
    let found = codes(&format!(
        "{BASE} class B extends A {{ pub fn f(self) {{ let g = || super.value(1); }} }} fn main() {{}}"
    ));
    assert_eq!(found, vec![ErrorCode::E1002]);
}

#[test]
fn a_local_named_super_is_a_value() {
    assert_codes("fn main() { let super = 3; let n: i64 = super + 1; }", &[]);
}

#[test]
fn unresolved_names_report_once_per_use_without_cascades() {
    for body in [
        "let a = missing + 1;",
        "let a: String = missing;",
        "let a = missing.len();",
        "take(missing);",
        "let a: bool = missing == 3;",
        "if missing {}",
        "let a = missing[0];",
        "let a = missing.field;",
    ] {
        let found = codes(&format!("fn take(s: String) {{}} fn main() {{ {body} }}"));
        assert_eq!(found, vec![ErrorCode::E0350], "{body}");
    }
    assert_codes(
        "fn main() { let a = missing; let b = missing + 1; println(missing); }",
        &[ErrorCode::E0350, ErrorCode::E0350, ErrorCode::E0350],
    );
}
