//! Bottom-type compatibility across value-consuming contexts (willow-9tls.53).
use super::{ErrorCode, check_source};

#[test]
fn never_value_contexts() {
    for source in [
        r#"class C { x: i64; init(self, flag: bool) { self.x = flag ? 1 : panic("stop"); } } fn main() {}"#,
        r#"class C { x: i64; init(self, flag: bool) { self.x = flag ? panic("stop") : 1; } } fn main() {}"#,
        r#"class C { x: i64; init(self) { self.x = panic("stop"); } } fn main() {}"#,
        r#"fn main() { let mut x: i64 = 1; x = panic("stop"); }"#,
        r#"import std::collections::Array; fn main() { let mut a: Array<i64> = [1]; a[0] = panic("stop"); }"#,
        r#"fn take(x: i64) {} fn main() { take(panic("stop")); }"#,
        r#"fn value() -> i64 { return panic("stop"); } fn main() {}"#,
        r#"fn main() { let x: i64 = true ? panic("a") : panic("b"); }"#,
    ] {
        let errors = check_source(source);
        assert!(errors.is_empty(), "{source}: {errors:?}");
    }
}

#[test]
fn never_does_not_initialize_recovered_field() {
    let errors = check_source(
        r#"
enum Option<T> { Some(T), None }
class C { x: i64; init(self) {
    defer match recover() { Some(_) => {}, None => {} };
    self.x = panic("stop");
} } fn main() {}
"#,
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, ErrorCode::E0842);
}

#[test]
fn never_preserves_concrete_mismatches_and_spans() {
    for (body, code, marked) in [
        (r#"let mut x: i64 = 0; x = "bad";"#, ErrorCode::E0201, "x"),
        (r#"let x = true ? 1 : "bad";"#, ErrorCode::E0902, r#""bad""#),
        (r#"take("bad");"#, ErrorCode::E0201, r#""bad""#),
        (r#"return "bad";"#, ErrorCode::E0201, "return"),
    ] {
        let source = format!("fn take(x: i64) {{}} fn f() -> i64 {{ {body} }} fn main() {{}}");
        let errors = check_source(&source);
        let error = errors
            .iter()
            .find(|error| error.code == code)
            .expect("type error");
        let span = error.labels[0].span;
        assert_eq!(&source[span.start..span.end], marked, "{errors:?}");
    }
}

#[test]
fn never_compatibility_is_directional() {
    use super::{Type, TypeChecker};
    let checker = TypeChecker::new();
    for ty in [
        Type::I64,
        Type::F64,
        Type::String,
        Type::Void,
        Type::Array(Box::new(Type::I64)),
    ] {
        assert!(checker.types_compatible(&ty, &Type::Never));
        assert!(!checker.types_compatible(&Type::Never, &ty));
        assert_eq!(
            checker.unify_ternary_types(&ty, &Type::Never),
            Some(ty.clone())
        );
        assert_eq!(checker.unify_ternary_types(&Type::Never, &ty), Some(ty));
    }
    assert_eq!(
        checker.unify_ternary_types(&Type::Never, &Type::Never),
        Some(Type::Never)
    );
}
