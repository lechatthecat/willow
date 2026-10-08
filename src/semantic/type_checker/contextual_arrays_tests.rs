use super::*;
use crate::{lexer::Lexer, parser::Parser};

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
        assert!(
            !errors.iter().any(|d| d.severity == Severity::Error),
            "{name}: {errors:?}"
        );
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
            assert!(
                !errors.iter().any(|d| d.severity == Severity::Error),
                "{errors:?}"
            );
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
        assert!(
            !checker.errors.iter().any(|d| d.severity == Severity::Error),
            "{:?}",
            checker.errors
        );
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

#[test]
fn buildgraph_contextual_match_twenty_perspectives() {
    use crate::parser::iter::{AstEvent, AstWalk};
    for scalar in ["i64", "f64", "bool", "String"] {
        for context in ["let", "return", "argument", "constructor", "assignment"] {
            let expr = "match true { true => [], false => [] }";
            let body = match context {
                "let" => format!("fn f() {{ let xs: Array<{scalar}> = {expr}; }}"),
                "return" => format!("fn f() -> Array<{scalar}> {{ return {expr}; }}"),
                "argument" => format!("fn g(xs: Array<{scalar}>) {{}} fn f() {{ g({expr}); }}"),
                "constructor" => format!(
                    "class H {{ pub xs: Array<{scalar}>; }} fn f() {{ let h = new H({expr}); }}"
                ),
                _ => format!("fn f() {{ let mut xs: Array<{scalar}> = []; xs = {expr}; }}"),
            };
            let source = format!("import std::collections::Array; {body}");
            let (program, parse) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(parse.is_empty(), "{parse:?}");
            let mut checker = TypeChecker::new();
            checker.check_program(&program);
            assert!(
                !checker.errors.iter().any(|d| d.severity == Severity::Error),
                "{context}/{scalar}: {:?}",
                checker.errors
            );
            for item in &program.items {
                if let Item::Function(f) = item {
                    for event in AstWalk::new(AstEvent::Block(&f.body)) {
                        if let AstEvent::Expr(e @ Expr::ArrayLiteral(..)) = event {
                            assert_ne!(
                                checker.expr_types[&e.id()],
                                Type::Array(Box::new(Type::Void)),
                                "{context}/{scalar}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn buildgraph_match_arm_work_is_linear() {
    use super::check_collections::ARRAY_ELEMENT_CHECKS;
    use super::check_lambda_match::EMPTY_ARRAY_REFINEMENTS;
    for n in [16, 64, 256, 1024] {
        let mut source =
            String::from("import std::collections::Array; fn f(n: i64) { let xs = match n {");
        for i in 0..n {
            source.push_str(&format!("{i} => [],"));
        }
        source.push_str("_ => [1] }; }");
        ARRAY_ELEMENT_CHECKS.with(|count| count.set(0));
        EMPTY_ARRAY_REFINEMENTS.with(|count| count.set(0));
        let errors = check_source(&source);
        assert!(
            !errors.iter().any(|d| d.severity == Severity::Error),
            "{errors:?}"
        );
        assert_eq!(ARRAY_ELEMENT_CHECKS.with(|count| count.get()), 1);
        assert_eq!(EMPTY_ARRAY_REFINEMENTS.with(|count| count.get()), n);
    }
}

#[test]
fn buildgraph_explicit_constructor_types_are_not_overridden() {
    for (ty, constructor) in [
        ("Map<String, i64>", "Map<i64, i64>::new()"),
        ("Map<String, i64>", "Map<String, String>::new()"),
        ("Map<String, i64>", "Map<i64>::new()"),
        ("Map<String, i64>", "Map<i64, i64, i64>::new()"),
        ("Map<String, i64>", "Map::new(1)"),
        ("Channel<String>", "Channel<i64>::new()"),
        ("Channel<String>", "Channel<i64>::with_capacity(1)"),
    ] {
        let source = format!(
            "import std::collections::Map; class H {{ pub value: {ty}; }} fn main() {{ let h = new H({constructor}); }}"
        );
        let errors = check_source(&source);
        assert!(
            errors.iter().any(|d| d.severity == Severity::Error),
            "{constructor}: expected a source error"
        );
        assert!(
            errors.iter().all(|d| d.code != ErrorCode::E0800),
            "{errors:?}"
        );
    }
}

#[test]
fn buildgraph_nested_empty_match_work_is_linear() {
    use super::check_lambda_match::EMPTY_ARRAY_REFINEMENTS;
    for n in [16, 64, 256, 1024] {
        let source = format!(
            "fn f() {{ let xs = match true {{ true => {}[]{} , false => [1] }}; }}",
            "match true { true => ".repeat(n),
            ", false => [] }".repeat(n)
        );
        EMPTY_ARRAY_REFINEMENTS.with(|count| count.set(0));
        let errors = check_source(&source);
        assert!(
            !errors.iter().any(|d| d.severity == Severity::Error),
            "{errors:?}"
        );
        assert_eq!(EMPTY_ARRAY_REFINEMENTS.with(|count| count.get()), 2 * n + 1);
    }
}

#[test]
fn freeze_inference_contexts() {
    // Two constructors across ten independent expected-type call paths.
    for (ty, value) in [
        ("FrozenMap<String, i64>", "Map::new().freeze()"),
        ("FrozenArray<i64>", "Array::new().freeze()"),
    ] {
        let cases = [
            format!("fn f() {{ let xs: {ty} = {value}; }}"),
            format!("fn f() -> {ty} {{ return {value}; }}"),
            format!("fn g(xs: {ty}) {{}} fn f() {{ g({value}); }}"),
            format!("class H {{ pub xs: {ty}; }} fn f() {{ let h = new H({value}); }}"),
            format!("class H {{ pub static xs: {ty} = {value}; }}"),
            format!("class H {{ pub xs: {ty}; pub fn set(self) {{ self.xs = {value}; }} }}"),
            format!("fn f() {{ let mut xs: {ty} = {value}; xs = {value}; }}"),
            format!("fn f(b: bool) -> {ty} {{ return b ? {value} : {value}; }}"),
            format!(
                "fn f(b: bool) -> {ty} {{ return match b {{ true => {value}, false => {value} }}; }}"
            ),
            format!("class H {{ pub fn g(self, xs: {ty}) {{}} }} fn f() {{ new H().g({value}); }}"),
        ];
        for (context, body) in cases.iter().enumerate() {
            let errors = check_source(&format!(
                "import std::collections::Map; import std::collections::Array; {body}"
            ));
            assert!(
                !errors.iter().any(|d| d.severity == Severity::Error),
                "{ty}/{context}: {errors:?}"
            );
        }
    }
}

#[test]
fn freeze_inference_preserves_errors() {
    for body in [
        "fn f() -> FrozenMap<String, i64> { return Map<i64, i64>::new().freeze(); }",
        "fn f() -> FrozenMap<String, i64> { return Map<String, String>::new().freeze(); }",
        "fn f() -> FrozenArray<String> { return Array<i64>::new().freeze(); }",
        "fn f(xs: Array<i64>) -> FrozenArray<String> { return xs.freeze(); }",
        "fn f(xs: Map<i64, i64>) -> FrozenMap<String, i64> { return xs.freeze(); }",
        "fn f() -> FrozenArray<i64> { return [true].freeze(); }",
        "fn f() -> FrozenMap<String, i64> { return Map::new().freeze(1); }",
        "fn f() -> FrozenArray<i64> { return Array::new().freeze(1); }",
        "fn f() -> FrozenArray<i64> { return Array::new(1).freeze(); }",
        "fn f() -> FrozenArray<i64> { return Array<i64, bool>::new().freeze(); }",
        "fn f() -> FrozenMap<String, i64> { return Map::new(1).freeze(); }",
        "fn f() -> FrozenMap<String, i64> { return Map<i64>::new().freeze(); }",
        "fn f() -> FrozenArray<i64> { return missing.freeze(); }",
        "fn f() -> FrozenArray<i64> { return [1].freeze().freeze(); }",
    ] {
        let errors = check_source(&format!(
            "import std::collections::Map; import std::collections::Array; {body}"
        ));
        assert!(
            errors.iter().any(|d| d.severity == Severity::Error),
            "{body}"
        );
        assert!(
            errors.iter().all(|d| !d.message.contains("FrozenMap<void")
                && !d.message.contains("FrozenArray<void")),
            "{body}: {errors:?}"
        );
    }
}

#[test]
fn freeze_inference_unresolved_has_one_diagnostic() {
    for value in [
        "Map::new().freeze()",
        "Array::new().freeze()",
        "[].freeze()",
    ] {
        let errors = check_source(&format!(
            "import std::collections::Map; import std::collections::Array; fn f() {{ let x: i64 = {value}; }}"
        ));
        let errors: Vec<_> = errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "{value}: {errors:?}");
        assert!(errors[0].message.contains("known"), "{errors:?}");
    }
}

#[test]
fn freeze_inference_nested_and_receiver_contexts() {
    for body in [
        "fn f(b: bool) -> FrozenMap<String, i64> { return (b ? Map::new() : Map::new()).freeze(); }",
        "fn f(b: bool) -> FrozenArray<i64> { return (b ? Array::new() : []).freeze(); }",
        "fn f(b: bool) -> FrozenMap<String, i64> { return (match b { true => Map::new(), false => Map::new() }).freeze(); }",
        "fn f() -> FrozenArray<FrozenMap<String, i64>> { return [Map::new().freeze()].freeze(); }",
        "fn f() -> FrozenArray<Array<i64>> { return [Array::new()].freeze(); }",
        "fn f() -> FrozenArray<i64> { return [1, 2].freeze(); }",
        "fn f() { let m = Map::new(); m.insert(1, true); let frozen = m.freeze(); }",
        "class C { pub fn freeze(self) -> FrozenArray<i64> { return [1].freeze(); } } fn f() -> FrozenArray<i64> { return new C().freeze(); }",
        "fn f() -> Array<i64> { return Array::new(); }",
        "fn f() { let xs = Array<i64>::new(); xs.push(1); }",
    ] {
        let errors = check_source(&format!(
            "import std::collections::Map; import std::collections::Array; {body}"
        ));
        assert!(
            !errors.iter().any(|d| d.severity == Severity::Error),
            "{body}: {errors:?}"
        );
    }
}

#[test]
fn freeze_inference_receiver_work_is_linear() {
    use super::check_collections::ARRAY_ELEMENT_CHECKS;
    for n in [8, 32, 128, 512] {
        for (shape, expression, count) in [
            ("wide", format!("[{}].freeze()", vec!["1"; n].join(",")), n),
            (
                "deep branches",
                format!(
                    "{}[1]{} .freeze()",
                    "(true ? ".repeat(n),
                    " : [1])".repeat(n)
                ),
                n + 1,
            ),
        ] {
            ARRAY_ELEMENT_CHECKS.with(|c| c.set(0));
            let errors = check_source(&format!(
                "import std::collections::Array; fn f() -> FrozenArray<i64> {{ return {expression}; }}"
            ));
            assert!(
                !errors.iter().any(|d| d.severity == Severity::Error),
                "{errors:?}"
            );
            let actual = ARRAY_ELEMENT_CHECKS.with(|c| c.get());
            assert_eq!(actual, count, "{shape}/{n}");
            eprintln!("freeze receiver elements: {shape} n={n} count={actual}");
        }
    }
}

#[test]
fn freeze_inference_records_call_targets_and_receiver_types() {
    use crate::parser::iter::{AstEvent, AstWalk};
    let source = "import std::collections::Array; import std::collections::Map; class C { pub fn freeze(self) -> FrozenArray<i64> { return [1].freeze(); } } fn f() -> FrozenArray<i64> { return new C().freeze(); } fn g() -> FrozenMap<String, i64> { return Map::new().freeze(); } fn h() -> FrozenArray<i64> { return Array::new().freeze(); }";
    let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty());
    let mut checker = TypeChecker::new();
    checker.capture_call_sites = true;
    checker.check_program(&program);
    assert!(
        !checker.errors.iter().any(|d| d.severity == Severity::Error),
        "{:?}",
        checker.errors
    );
    let mut freezes = 0;
    for item in &program.items {
        if let Item::Function(f) = item {
            for event in AstWalk::new(AstEvent::Block(&f.body)) {
                if let AstEvent::Expr(Expr::MethodCall(call)) = event {
                    freezes += 1;
                    assert!(checker.analysis_calls[&call.id].is_some());
                    let receiver = &checker.expr_types[&call.object.id()];
                    assert!(!format!("{receiver:?}").contains("Void"));
                }
            }
        }
    }
    assert_eq!(freezes, 3);
    let sites = &checker.resolved_calls[&FunctionId::free_from_source_name("f")];
    assert!(
        sites
            .targets
            .contains(&FunctionId::method(TypeId::local("C"), "freeze"))
    );
}

#[test]
fn freeze_inference_deep_type_storage_is_explicit() {
    use super::check_collections::ARRAY_ELEMENT_CHECKS;
    for depth in [8, 16, 32, 64] {
        let ty = format!("{}i64{}", "FrozenArray<".repeat(depth), ">".repeat(depth));
        let value = format!("{}1{}", "[".repeat(depth), "].freeze()".repeat(depth));
        let source =
            format!("import std::collections::Array; fn f() -> {ty} {{ return {value}; }}");
        let (program, parse) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(parse.is_empty());
        ARRAY_ELEMENT_CHECKS.with(|c| c.set(0));
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert!(
            !checker.errors.iter().any(|d| d.severity == Severity::Error),
            "{:?}",
            checker.errors
        );
        assert_eq!(ARRAY_ELEMENT_CHECKS.with(|c| c.get()), depth);
        let mut nodes = 0;
        for mut ty in checker.expr_types.values() {
            loop {
                match ty {
                    Type::Array(element) => ty = element,
                    Type::Generic(name, args) if name == "FrozenArray" => ty = &args[0],
                    _ => break,
                }
                nodes += 1;
            }
        }
        assert_eq!(nodes, depth * (depth + 1));
        eprintln!("nested freezes depth={depth} element_checks={depth} owned_type_nodes={nodes}");
    }
}
