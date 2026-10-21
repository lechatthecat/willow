use super::*;

fn checked(source: &str) -> TypeChecker {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = TypeChecker::new();
    crate::register_prelude(&mut checker).unwrap();
    checker.check_program(&program);
    checker
}

fn check(source: &str) -> Vec<Diagnostic> {
    checked(source).errors
}

#[test]
fn generic_variant_payload_and_later_argument() {
    for source in [
        "fn f<T>(o: Option<T>) -> T { return o.unwrap(); } fn main() { let x = f(Some(4)); }",
        "fn f<T>(o: Option<T>, d: T) -> T { return d; } fn main() { let x = f(None, 4); }",
        "fn f<T>(r: Result<T, String>) -> bool { return r.is_ok(); } fn main() { let x = f(Ok(4)); }",
    ] {
        let errors = check(source);
        assert!(errors.is_empty(), "{source}: {errors:?}");
    }
}

const DECLS: &str = "
fn opt<T>(o: Option<T>) -> T { return o.unwrap(); }
fn fallback<T>(o: Option<T>, d: T) -> T { return d; }
fn reverse<T>(d: T, o: Option<T>) -> T { return d; }
fn result<T>(r: Result<T, String>) -> bool { return r.is_ok(); }
fn both<T,E>(r: Result<T,E>, value: T, error: E) -> bool { return r.is_ok(); }
fn nested<T>(o: Option<Option<T>>, d: T) -> T { return d; }
";

#[track_caller]
fn clean(body: &str) {
    let errors = check(&format!("{DECLS} fn main() {{ {body} }}"));
    assert!(errors.is_empty(), "{body}: {errors:?}");
}

macro_rules! positive {
    ($($name:ident: $body:literal),* $(,)?) => { $(
        #[test] fn $name() { clean($body); }
    )* };
}
positive! {
    p01_some_integer: "let x: i64 = opt(Some(4));",
    p02_some_string: "let x: String = opt(Some(\"hi\"));",
    p03_some_bool: "let x: bool = opt(Some(true));",
    p04_none_later: "let x: i64 = fallback(None, 4);",
    p05_none_earlier: "let x: i64 = reverse(4, None);",
    p06_some_later: "let x: i64 = fallback(Some(2), 4);",
    p07_ok_infers_t: "let x: bool = result(Ok(4));",
    p08_err_other_args: "let x: bool = both(Err(\"bad\"), 4, \"fallback\");",
    p09_ok_other_args: "let x: bool = both(Ok(4), 3, \"fallback\");",
    p10_explicit_none: "let x: i64 = fallback<i64>(None, 4);",
    p11_qualified_some: "let x: i64 = opt(Option::Some(4));",
    p12_qualified_none: "let x: i64 = fallback(Option::None, 4);",
    p13_qualified_ok: "let x: bool = result(Result::Ok(4));",
    p14_nested_some: "let x: i64 = nested(Some(Some(4)), 3);",
    p15_nested_none: "let x: i64 = nested(Some(None), 3);",
    p16_outer_none: "let x: i64 = nested(None, 3);",
    p17_lambda_return: "let g: closure(i64) -> Option<i64> = |a: i64| Some(a+1);",
    p18_qualified_without_context: "let x = Option::Some(4);",
    p19_concrete_context: "let x: Option<i64> = Some(4);",
}

#[test]
fn p20_missing_context_diagnostics() {
    for (expression, name, qualified) in [
        ("Some(4)", "Some", "Option::Some(..)"),
        ("None", "None", "Option::None"),
        ("Ok(4)", "Ok", "Result::Ok(..)"),
        ("Err(4)", "Err", "Result::Err(..)"),
    ] {
        let errors = check(&format!("fn main() {{ let x = {expression}; }}"));
        assert_eq!(errors.len(), 1, "{errors:?}");
        let text = format!("{:?}", errors[0]);
        assert!(
            text.contains(&format!("shorthand `{name}` needs an expected")),
            "{text}"
        );
        assert!(
            text.contains(qualified) && text.contains("annotation"),
            "{text}"
        );
    }
}

#[test]
fn p21_generic_mismatch_details() {
    let errors = check(&format!(
        "{DECLS} fn main() {{ let x = fallback(Some(4), true); }}"
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    let text = &errors[0].message;
    for part in ["argument 2", "`d`", "expected", "found `bool`"] {
        assert!(text.contains(part), "{text}");
    }
}

#[test]
fn p22_unresolved_none_remains_error() {
    let errors = check(&format!("{DECLS} fn main() {{ let x = opt(None); }}"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .message
            .contains("cannot infer type parameter `T`")
    );
}

#[test]
fn p23_ordinary_unknown_name() {
    let errors = check("fn main() { let x = Missing(4); }");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find function `Missing`"));
}

#[test]
fn p24_shadowed_function_is_ordinary() {
    let errors = check("fn Some(x: i64) -> i64 { return x; } fn main() { let x: i64 = Some(4); }");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn p25_repeated_context_visits_scale_linearly() {
    for n in [8, 16, 32, 64] {
        let calls = (0..n)
            .map(|i| format!("let x{i} = fallback(Some({i}), 0);"))
            .collect::<String>();
        generics::GENERIC_ENUM_ARGUMENT_CHECKS.with(|c| c.set(0));
        clean(&calls);
        let visits = generics::GENERIC_ENUM_ARGUMENT_CHECKS.with(|c| c.get());
        println!("calls={n} contextual_visits={visits}");
        assert_eq!(visits, 3 * n);
    }
}

#[test]
fn p26_wrong_variant_payload_details() {
    let errors = check(&format!(
        "{DECLS} fn main() {{ let x = both(Err(true), 1, \"bad\"); }}"
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("argument 3"), "{errors:?}");
}

#[test]
fn p27_missing_payload_rejected() {
    let errors = check(&format!(
        "{DECLS} fn main() {{ let x = fallback(Some(), 4); }}"
    ));
    assert!(!errors.is_empty());
    assert!(
        errors[0].message.contains("takes 1 argument(s), got 0"),
        "{errors:?}"
    );
}

#[test]
fn p28_deep_constructor_visits_match_depth() {
    for depth in [4, 8, 16, 32] {
        let ty = format!("{}T{}", "Option<".repeat(depth), ">".repeat(depth));
        let value = format!("{}1{}", "Some(".repeat(depth), ")".repeat(depth));
        let source = format!(
            "fn deep<T>(v: {ty}, d: T) -> T {{ return d; }} fn main() {{ let x = deep({value}, 0); }}"
        );
        generics::GENERIC_ENUM_ARGUMENT_CHECKS.with(|c| c.set(0));
        let errors = check(&source);
        assert!(errors.is_empty(), "{errors:?}");
        let visits = generics::GENERIC_ENUM_ARGUMENT_CHECKS.with(|c| c.get());
        println!("depth={depth} contextual_visits={visits}");
        assert_eq!(visits, depth + 2);
    }
}

#[test]
fn p29_nested_none_metadata_is_concrete() {
    let checker = checked(
        "fn f<T>(o: Option<Option<T>>, d: T) -> T { return d; } fn main() { let x = f(Some(None), 4); }",
    );
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    for ty in checker.expr_types.values() {
        if matches!(ty, Type::Generic(name, _) if name == "Option") {
            assert!(
                !format!("{ty:?}").contains("Void"),
                "unresolved constructor: {ty:?}"
            );
        }
    }
}

#[test]
fn p30_void_success_payload_remains_omittable() {
    let errors = check(
        "fn f<E>(r: Result<void, E>, e: E) -> bool { return r.is_ok(); } fn main() { let x = f(Ok(), \"bad\"); }",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn review_concrete_void_is_not_a_hole() {
    let errors = check(
        "fn f<T>(o: Option<Result<i64,T>>, e: T) -> bool { return true; } fn main() { let r: Result<void,String> = Ok(); let x = f(Some(r), \"error\"); }",
    );
    assert!(!errors.is_empty(), "concrete void was accepted as i64");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("expected") && e.message.contains("found")),
        "{errors:?}"
    );
}

#[test]
fn review_partial_context_reaches_branches() {
    for body in [
        "let x: i64 = fallback(true ? Some(1) : None, 2);",
        "let x: i64 = fallback(match true { true => Some(1), false => None }, 2);",
        "let x: i64 = fallback(true ? None : None, 2);",
        "let x: i64 = fallback(match true { true => None, false => None }, 2);",
        "let x: i64 = nested(true ? Some(None) : None, 2);",
        "let x: i64 = fallback(true ? (false ? None : Some(1)) : None, 2);",
    ] {
        clean(body);
    }
}

#[test]
fn review_concrete_void_in_branches_stays_concrete() {
    for expr in [
        "true ? Some(r) : None",
        "match true { true => Some(r), false => None }",
        "Some(true ? r : r)",
        "Some(Result<void,String>::Ok())",
    ] {
        let errors = check(&format!(
            "fn f<T>(o: Option<Result<i64,T>>, e: T) -> bool {{ return true; }} fn main() {{ let r: Result<void,String> = Ok(); let x = f({expr}, \"error\"); }}"
        ));
        assert!(!errors.is_empty(), "accepted {expr}");
    }
    // A concrete void field can neither be skipped nor merged with a sibling i64.
    for expr in ["true ? r : s", "match true { true => r, false => s }"] {
        let errors = check(&format!(
            "fn f<T>(r: Result<T,String>, d: T) -> bool {{ return true; }} fn main() {{ let r: Result<void,String> = Ok(); let s: Result<i64,String> = Ok(1); let x = f({expr}, 2); }}"
        ));
        assert!(!errors.is_empty(), "accepted {expr}");
    }
}

#[test]
fn review_branches_preserve_void_success_and_result_inference() {
    for expr in [
        "true ? Ok() : Err(\"bad\")",
        "match true { true => Ok(), false => Err(\"bad\") }",
    ] {
        let errors = check(&format!(
            "fn f<E>(r: Result<void,E>, e: E) -> bool {{ return r.is_ok(); }} fn main() {{ let x = f({expr}, \"error\"); }}"
        ));
        assert!(errors.is_empty(), "{expr}: {errors:?}");
    }
    clean("let x = both(true ? Ok(1) : Err(\"bad\"), 2, \"fallback\");");
    clean("let x = both(match true { true => Ok(1), false => Err(\"bad\") }, 2, \"fallback\");");
}

#[test]
fn review_branch_metadata_has_no_holes() {
    for expr in [
        "true ? Some(None) : None",
        "match true { true => Some(None), false => None }",
        "Some(match true { true => None, false => Some(1) })",
    ] {
        let checker = checked(&format!(
            "{DECLS} fn main() {{ let x = nested({expr}, 2); }}"
        ));
        assert!(checker.errors.is_empty(), "{expr}: {:?}", checker.errors);
        assert!(
            checker
                .expr_types
                .values()
                .all(|ty| !crate::semantic::generic_inference::has_enum_hole(ty)),
            "metadata hole in {expr}"
        );
    }
}

#[test]
fn review_unresolved_branches_report_inference_error() {
    for expr in [
        "true ? None : None",
        "match true { true => None, false => None }",
    ] {
        let errors = check(&format!("{DECLS} fn main() {{ let x = opt({expr}); }}"));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("cannot infer type parameter"),
            "{errors:?}"
        );
        assert!(!format!("{errors:?}").contains("enum-inference-hole"));
    }
}

#[test]
fn review_match_fanout_is_linear() {
    for n in [8, 16, 32, 64] {
        let arms = (0..n)
            .map(|i| format!("{i} => Some({i}),"))
            .collect::<String>();
        generics::GENERIC_ENUM_ARGUMENT_CHECKS.with(|c| c.set(0));
        clean(&format!(
            "let x = fallback(match 1 {{ {arms} _ => None }}, 2);"
        ));
        let visits = generics::GENERIC_ENUM_ARGUMENT_CHECKS.with(|c| c.get());
        println!("match_arms={n}+1 contextual_visits={visits}");
        assert_eq!(visits, 2 * n + 3);
    }
}
