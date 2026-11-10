use super::check_source;

#[test]
fn contracts_reference_and_type_restrictions() {
    // Each clause is checked independently, including statically false paths.
    for (name, source) in [
        ("requires result", "fn f(x:i64) requires result > 0 {}"),
        ("void result", "fn f() ensures result > 0 {}"),
        ("body local", "fn f() ensures x > 0 { let x = 1; }"),
        ("reference parameter", "fn f(x: &i64) requires x > 0 {}"),
        (
            "mutable object",
            "class C { pub x:i64; } fn change(c:C){c.x=1;} fn f(c:C) requires c.x > 0 {}",
        ),
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

#[test]
fn loop_annotations_types_scope_and_effects() {
    for (name, annotations, body) in [
        ("integer invariant", "invariant 1 variant x", "x=x-1;"),
        ("bool variant", "invariant true variant true", "x=x-1;"),
        ("float variant", "invariant true variant 1.0", "x=x-1;"),
        ("body local", "invariant y>=0 variant x", "let y=1;x=x-1;"),
        ("implicit result", "invariant result>=0 variant x", "x=x-1;"),
        (
            "unknown binding",
            "invariant missing>=0 variant x",
            "x=x-1;",
        ),
        ("call", "invariant g() variant x", "x=x-1;"),
        ("variant call", "invariant true variant h()", "x=x-1;"),
        ("heap", "invariant [1]==[1] variant x", "x=x-1;"),
        (
            "unreachable bad clause",
            "invariant false invariant 1 variant x",
            "x=x-1;",
        ),
        (
            "bad operator",
            "invariant false && (1+true>0) variant x",
            "x=x-1;",
        ),
    ] {
        let source = format!(
            "fn g()->bool {{return true;}} fn h()->i64 {{return 1;}} fn f() {{let mut x=2;while x>0 {annotations} {{{body}}}}} fn main() {{}}"
        );
        let errors = check_source(&source);
        assert!(!errors.is_empty(), "{name}: {source}");
    }
    let valid = "const N:i64=2; fn f(){let mut x=N;while x>0 invariant x>=0 variant x {let y=x;x=y-1;}} fn main(){}";
    let errors = check_source(valid);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn contracts_immutable_reads_and_quantifier_rejections() {
    for source in [
        "class C { pub x:i64; } fn f(c:C)->i64 requires c.x>0 ensures result==c.x {return c.x;} fn main(){}",
        "fn f(a:FrozenArray<i64>)->bool requires forall(0,a.len(),|i:i64|a[i]>=0) ensures result {return true;} fn main(){}",
    ] {
        let errors = check_source(source);
        assert!(errors.is_empty(), "{errors:?}");
    }
    for source in [
        "class C {pub fn len(self)->i64 {println(1);return 0;}} fn f(c:C) requires c.len()==0 {} fn main(){}",
        "fn f() requires forall(0,1,|i:i64|i) {} fn main(){}",
        "fn f() requires forall(0,1,|i:bool|i) {} fn main(){}",
        "fn f() requires exists(0,1,true) {} fn main(){}",
        "fn f() requires forall(0,1,|i:i64|{return true;}) {} fn main(){}",
        "fn forall()->bool{return true;} fn main(){}",
    ] {
        let errors = check_source(source);
        assert!(
            errors.iter().any(|d| d.code == super::ErrorCode::E2631),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn contracts_match_statement_arms_rejected() {
    for clause in ["requires", "ensures"] {
        for statement in ["return true;", "C::n = 1; return true;"] {
            for quantified in [false, true] {
                let expression = format!("match b {{ true => {{ {statement} }}, false => true }}");
                let expression = if quantified {
                    format!("forall(0,1,|i:i64|exists(0,1,|j:i64|{expression}))")
                } else {
                    expression
                };
                let source = format!(
                    "class C {{ pub static n:i64 = 0; }} @pure fn f(b:bool)->bool {clause} {expression} {{ return true; }} fn main() {{}}"
                );
                let errors = check_source(&source);
                assert!(
                    errors.iter().any(|d| d.code == super::ErrorCode::E2631),
                    "{source}: {errors:?}"
                );
            }
        }
        for expression in [
            "match b { true => true, false => false }",
            "forall(0,1,|i:i64|exists(0,1,|j:i64|match b { true => i==j, false => true }))",
        ] {
            let source = format!(
                "@pure fn f(b:bool)->bool {clause} {expression} {{ return true; }} fn main() {{}}"
            );
            let errors = check_source(&source);
            assert!(errors.is_empty(), "{source}: {errors:?}");
        }
    }
}
