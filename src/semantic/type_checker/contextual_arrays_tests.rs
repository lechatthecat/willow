use super::*;

const PREFIX: &str = r#"
import std::collections::Array;
interface Search { fn value(self) -> i64; }
class A implements Search { pub fn value(self) -> i64 { return 1; } }
class B implements Search { pub fn value(self) -> i64 { return 2; } }
class Rock {}
"#;

#[test]
fn contextual_arrays_positive_perspectives() {
    let cases = [
        (
            "return",
            "fn f() -> Array<Search> { return [new A(), new B()]; }",
        ),
        (
            "let",
            "fn f() { let xs: Array<Search> = [new A(), new B()]; }",
        ),
        (
            "argument",
            "fn g(xs: Array<Search>) {} fn f() { g([new A(), new B()]); }",
        ),
        (
            "memberwise field",
            "class H { pub xs: Array<Search>; } fn f() { let h = new H([new A(), new B()]); }",
        ),
        (
            "static field",
            "class H { pub static xs: Array<Search> = [new A(), new B()]; }",
        ),
        (
            "field assignment",
            "class H { pub xs: Array<Search>; pub fn set(self) { self.xs = [new A(), new B()]; } }",
        ),
        (
            "nested",
            "fn f() -> Array<Array<Search>> { return [[new A(), new B()], [new B()]]; }",
        ),
        ("empty", "fn f() -> Array<Search> { return []; }"),
        ("singleton", "fn f() -> Array<Search> { return [new A()]; }"),
        (
            "ternary",
            "fn f(b: bool) -> Array<Search> { return b ? [new A()] : [new B()]; }",
        ),
        (
            "match",
            "fn f(b: bool) -> Array<Search> { return match b { true => [new A()], false => [new B()] }; }",
        ),
        (
            "method argument",
            "class H { pub fn g(self, xs: Array<Search>) {} } fn f() { new H().g([new A(), new B()]); }",
        ),
        (
            "static argument",
            "class H { pub static fn g(xs: Array<Search>) {} } fn f() { H::g([new A(), new B()]); }",
        ),
        (
            "already interface",
            "fn f(a: Search) -> Array<Search> { return [a, new B()]; }",
        ),
        (
            "homogeneous inference",
            "fn f() { let xs = [new A(), new A()]; }",
        ),
        ("scalar", "fn f() -> Array<i64> { return [1, 2]; }"),
        (
            "nested empty",
            "fn f() -> Array<Array<Search>> { return [[], [new A(), new B()]]; }",
        ),
        (
            "parentheses",
            "fn f() -> Array<Search> { return ([new A(), new B()]); }",
        ),
        (
            "reversed",
            "fn f() -> Array<Search> { return [new B(), new A()]; }",
        ),
        (
            "array push",
            "fn f() { let xs: Array<Array<Search>> = []; xs.push([new A(), new B()]); }",
        ),
    ];
    for (name, body) in cases {
        let errors = check_source(&format!("{PREFIX}{body}"));
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }
}

#[test]
fn contextual_arrays_reject_at_bad_element() {
    for body in [
        "fn f() -> Array<Search> { return [new A(), new Rock()]; }",
        "fn f() { let xs: Array<Search> = [new A(), new Rock()]; }",
        "fn g(xs: Array<Search>) {} fn f() { g([new A(), new Rock()]); }",
        "class H { pub xs: Array<Search>; } fn f() { let h = new H([new A(), new Rock()]); }",
        "class H { pub static xs: Array<Search> = [new A(), new Rock()]; }",
        "fn f() -> Array<Search> { return [new Rock(), new B()]; }",
        "fn f() -> Array<Array<Search>> { return [[new A(), new Rock()]]; }",
    ] {
        let source = format!("{PREFIX}{body}");
        let errors = check_source(&source);
        let errors: Vec<_> = errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "{body}: {errors:?}");
        assert_eq!(errors[0].code, ErrorCode::E0201);
        assert_eq!(
            errors[0].primary_span().unwrap().start,
            source.find("new Rock()").unwrap()
        );
    }
}

#[test]
fn contextual_arrays_unannotated_help() {
    let errors = check_source(&format!(
        "{PREFIX}fn f() {{ let xs = [new A(), new B()]; }}"
    ));
    let error = errors.iter().find(|e| e.code == ErrorCode::E0201).unwrap();
    assert!(error.message.contains("expected `A`, found `B`"));
    assert!(
        error
            .helps
            .iter()
            .any(|h| h.contains("Array<InterfaceName>"))
    );
}

#[test]
fn contextual_arrays_element_checks_scale_linearly() {
    use super::check_collections::ARRAY_ELEMENT_CHECKS;
    for n in [8, 16, 32, 64] {
        for (shape, ty, literal, expected_visits) in [
            (
                "wide",
                "Array<Search>".to_string(),
                format!("[{}]", vec!["new A(), new B()"; n].join(",")),
                2 * n,
            ),
            (
                "nested fanout",
                "Array<Array<Search>>".to_string(),
                format!("[{}]", vec!["[new A(), new B()]"; n].join(",")),
                3 * n,
            ),
            (
                "deep",
                format!("{}Search{}", "Array<".repeat(n), ">".repeat(n)),
                format!("{}new A(){}", "[".repeat(n), "]".repeat(n)),
                n,
            ),
        ] {
            ARRAY_ELEMENT_CHECKS.with(|count| count.set(0));
            let errors = check_source(&format!("{PREFIX}fn f() -> {ty} {{ return {literal}; }}"));
            assert!(errors.is_empty(), "{errors:?}");
            let actual = ARRAY_ELEMENT_CHECKS.with(|count| count.get());
            assert_eq!(actual, expected_visits, "{shape} n={n}");
            eprintln!("array element checks: {shape} n={n}: {actual}");
        }
    }
}

#[test]
fn contextual_arrays_deep_recorded_type_size() {
    for depth in [8, 16, 32, 64] {
        let ty = format!("{}Search{}", "Array<".repeat(depth), ">".repeat(depth));
        let value = format!("{}new A(){}", "[".repeat(depth), "]".repeat(depth));
        let source = format!("{PREFIX}fn f() -> {ty} {{ return {value}; }}");
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let mut recorded_array_nodes = 0;
        for mut ty in checker.expr_types.values() {
            while let Type::Array(element) = ty {
                recorded_array_nodes += 1;
                ty = element;
            }
        }
        assert_eq!(recorded_array_nodes, depth * (depth + 1) / 2);
        eprintln!("owned array type nodes: depth={depth}: {recorded_array_nodes}");
    }
}
