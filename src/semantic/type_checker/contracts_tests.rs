use super::check_source;

#[test]
fn contracts_reference_and_type_restrictions() {
    // Each clause is checked independently, including statically false paths.
    for (name, source) in [
        ("requires result", "fn f(x:i64) requires result > 0 {}"),
        ("void result", "fn f() ensures result > 0 {}"),
        ("body local", "fn f() ensures x > 0 { let x = 1; }"),
        ("reference parameter", "fn f(x: &i64) requires x > 0 {}"),
        ("object", "class C { x:i64; } fn f(c:C) requires c.x > 0 {}"),
        ("array", "fn f(a:Array<i64>) requires a[0] > 0 {}"),
        ("call", "fn g()->bool {return true;} fn f() requires g() {}"),
        ("method", "fn f(x:i64) requires x.abs() > 0 {}"),
        (
            "assignment target shadow",
            "fn f(result:i64) ensures result > 0 {}",
        ),
        ("integer condition", "fn f() requires 1 {}"),
        ("independent clauses", "fn f() requires false requires 1 {}"),
        (
            "invalid short circuit",
            "fn f() requires false && (1 + true > 0) {}",
        ),
        ("allocation", "fn f() requires [1] == [1] {}"),
        ("string reference", "fn f(s:String) requires s == s {}"),
    ] {
        let errors = check_source(&format!("{source} fn main() {{}}"));
        assert!(
            errors.iter().any(|d| (name == "invalid short circuit"
                && d.code == super::ErrorCode::E0202)
                || d.message.contains("contract")
                || d.message.contains("result is reserved")
                || d.labels
                    .iter()
                    .any(|label| label.message == "invalid contract expression")),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn contracts_scalar_constants_and_separate_result_scope() {
    let source = "const LIMIT:i64 = 9; fn f(x:i64, ok:bool, y:f64)->i64 requires ok requires y >= 0.0 requires x < LIMIT ensures result == x { let result = x; return result; } fn main() {}";
    let errors = check_source(source);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn contracts_body_cannot_see_implicit_result() {
    assert!(
        !check_source("fn f()->i64 ensures result > 0 { return result; } fn main() {}").is_empty()
    );
}
