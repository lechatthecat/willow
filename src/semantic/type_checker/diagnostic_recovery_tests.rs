use super::*;

#[test]
fn diagnostic_recovery_method_uses() {
    for use_site in [
        "code.len();",
        "code.nope();",
        "let alias = code; alias.len();",
        "let n: i64 = code;",
        "take(code);",
        "let n = code + 1;",
        "let n = -code;",
        "let n = code[0];",
        "let n = code.field;",
        "let n = code.len().other();",
        "let n = [code];",
        "println(code);",
        "for item in code { item.len(); }",
        "if code {}",
        "while code {}",
        "let n = code ? 1 : 2;",
    ] {
        let source = format!(
            "import std::collections::Array; fn take(n: i64) {{}} fn main() {{ let input = [1, 2]; let code = input.clone(); {use_site} }}"
        );
        let errors = check_source(&source);
        assert_eq!(errors.len(), 1, "{use_site}: {errors:?}");
        assert!(errors[0].message.contains("clone"), "{errors:?}");
    }
}

#[test]
fn diagnostic_recovery_preserves_independent_errors() {
    for body in [
        "let n: i64 = true;",
        "let n = 1.nope();",
        "let n = absent;",
        "code.other(absent);",
        "let n = code[true];",
    ] {
        let errors = check_source(&format!(
            "import std::collections::Array; fn main() {{ let code = [1].clone(); {body} }}"
        ));
        assert!(errors.len() >= 2, "{body}: {errors:?}");
    }
}

#[test]
fn diagnostic_recovery_missing_type_compatibility() {
    for annotation in [
        "Missing",
        "Array<Missing>",
        "Result<i64, Missing>",
        "Option<Missing>",
    ] {
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        let tokens = crate::lexer::Lexer::new(&format!(
            "import std::collections::Array; fn take(x: {annotation}) {{}} fn main() {{}}"
        ))
        .tokenize()
        .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        checker.check_program(&program);
        assert!(
            checker
                .errors
                .iter()
                .any(|e| e.message.contains("cannot find type `Missing`")),
            "{:?}",
            checker.errors
        );
        assert!(checker.types_compatible(
            &Type::Generic(
                "Result".into(),
                vec![Type::I64, Type::Named("Missing".into())]
            ),
            &Type::Generic("Result".into(), vec![Type::I64, Type::String])
        ));
        assert!(!checker.types_compatible(
            &Type::Generic(
                "Result".into(),
                vec![Type::Bool, Type::Named("Missing".into())]
            ),
            &Type::Generic("Result".into(), vec![Type::I64, Type::String])
        ));
    }
}

#[test]
fn diagnostic_recovery_real_void_still_errors() {
    let errors = check_source("fn empty() {} fn main() { let value = empty(); value.len(); }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("void") && e.message.contains("no methods")),
        "{errors:?}"
    );
}

#[test]
fn diagnostic_recovery_import_visibility_and_shared_index() {
    let source = "pub enum Trap { Underflow } enum Secret { Hidden } pub class Machine {} pub interface Device { fn run(self); }";
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty());
    let mut checker = TypeChecker::new();
    checker.set_module_path("vm");
    checker.check_module_program(&program);
    assert!(
        checker
            .unknown_type_help("Trap")
            .contains("import vm::Trap;")
    );
    assert!(
        !checker
            .unknown_type_help("Secret")
            .contains("import vm::Secret")
    );
    assert!(std::rc::Rc::ptr_eq(
        &checker.import_type_hints,
        &checker.fork_body().import_type_hints
    ));
}

#[test]
fn diagnostic_recovery_fanout_counts() {
    for n in [16, 64, 256] {
        let mut source =
            "import std::collections::Array; fn main() { let code = [1].clone();".to_owned();
        for i in 0..n {
            source.push_str(&format!("let x{i} = code.len();"));
        }
        source.push('}');
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&program);
        assert_eq!(checker.errors.len(), 1);
        assert_eq!(checker.expr_types.len(), 2 * n + 3);
    }
}

#[test]
fn ticket_52_type_typo_spans_and_hints() {
    for (ty, expected) in [
        ("Arry", "Array"),
        ("FrozenArry<i64>", "FrozenArray"),
        ("Chanel<i64>", "Channel"),
        ("Strng", "String"),
    ] {
        for binding in ["let y", "let mut y"] {
            let source = format!("fn main() {{ {binding}: {ty} = 1; }}");
            let errors = check_source(&source);
            let error = errors
                .iter()
                .find(|d| d.message.starts_with("cannot find type"))
                .unwrap();
            assert!(
                error
                    .helps
                    .iter()
                    .any(|h| h.contains(&format!("did you mean `{expected}`"))),
                "{error:?}"
            );
            let span = error.labels[0].span;
            assert_eq!(&source[span.start..span.end], ty);
        }
    }
}

#[test]
fn ticket_52_channel_capacity_hint() {
    for call in [
        "Channel::new(1)",
        "Channel<i64>::new(1)",
        "Channel::new(1, 2)",
    ] {
        let errors = check_source(&format!("fn main() {{ let c = {call}; }}"));
        let error = errors
            .iter()
            .find(|d| d.message.contains("expects 0 arguments"))
            .unwrap();
        assert!(
            error
                .helps
                .iter()
                .any(|h| h.contains("Channel::with_capacity(n)"))
        );
    }
    for call in ["Channel<i64>::new()", "Channel<i64>::with_capacity(1)"] {
        assert!(check_source(&format!("fn main() {{ let c = {call}; }}")).is_empty());
    }
}

#[test]
fn ticket_52_typo_work_counts() {
    let checker = TypeChecker::new();
    // Built-in candidate lengths total 48; no scan over program declarations.
    for length in [8, 64, 512] {
        for repetitions in [1, 8, 64] {
            diagnostics::EDIT_CELLS.with(|cells| cells.set(0));
            for _ in 0..repetitions {
                checker.unknown_type_help(&"x".repeat(length));
            }
            let cells = diagnostics::EDIT_CELLS.with(|cells| cells.get());
            assert_eq!(cells, length * repetitions * 48);
            println!("length={length} repetitions={repetitions} cells={cells}");
        }
    }
}

#[test]
fn ticket_52_annotation_span_preserves_file_identity() {
    for ty in ["Arry", "FrozenArry<i64>", "Option<Arry>"] {
        let source = format!("fn main() {{ let value: {ty}=1; }}");
        let mut tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        for token in &mut tokens {
            token.span.file_id = crate::diagnostics::FileId(7);
        }
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        let error = checker
            .errors
            .iter()
            .find(|d| d.message.starts_with("cannot find type"))
            .unwrap();
        let span = error.labels[0].span;
        assert_eq!(span.file_id, crate::diagnostics::FileId(7));
        assert_eq!(&source[span.start..span.end], ty);
    }
}

#[test]
fn closure_field_call_help_perspectives() {
    for (class, ty) in [("Plain", "i64"), ("Box<T>", "T")] {
        for callable in ["closure", "fn"] {
            let call = "self.f(x)";
            let source = format!(
                "class {class} {{ f: {callable}({ty}) -> {ty}; pub fn run(self, x: {ty}) -> {ty} {{ return {call}; }} }} fn main() {{}}"
            );
            let errors = check_source(&source);
            let error = errors
                .iter()
                .find(|d| d.code == ErrorCode::E0502)
                .unwrap_or_else(|| panic!("{source}: {errors:?}"));
            assert!(
                error
                    .helps
                    .iter()
                    .any(|h| h.contains("let callback = receiver.f; callback(...)")),
                "{source}: {errors:?}"
            );
            assert!(
                error
                    .labels
                    .iter()
                    .any(|l| l.message == "callable field declared here"),
                "{errors:?}"
            );
        }
    }
}

#[test]
fn closure_field_help_does_not_replace_missing_method_diagnostic() {
    for field in ["", "f: i64;"] {
        let errors = check_source(&format!(
            "class Plain {{ {field} pub fn run(self) {{ self.f(); }} }} fn main() {{}}"
        ));
        let error = errors.iter().find(|d| d.code == ErrorCode::E0502).unwrap();
        assert!(!error.helps.iter().any(|h| h.contains("callable field")));
    }
}

#[test]
fn closure_field_help_inherited_declaration() {
    let source = "open class Base { pub f: closure(i64) -> i64; } class Child extends Base { pub fn run(self) { self.f(1); } } fn main() {}";
    let errors = check_source(source);
    let error = errors.iter().find(|d| d.code == ErrorCode::E0502).unwrap();
    let label = error
        .labels
        .iter()
        .find(|l| l.message == "callable field declared here")
        .unwrap();
    assert_eq!(&source[label.span.start..label.span.end], "f");
    assert!(error.helps.iter().any(|h| h.contains("receiver.f")));
}

#[test]
fn closure_field_parentheses_require_field_read() {
    let errors = check_source(
        "class Plain { pub fn f(self, x: i64) -> i64 { return x; } pub fn run(self) -> i64 { return (self.f)(1); } } fn main() {}",
    );
    assert!(!errors.is_empty(), "a method is not a field");
}

#[test]
fn sentinel_diagnostics_contextual_fanout_counts() {
    for n in [1, 16, 64, 256] {
        let mut source = "fn main() { let bad = absent;".to_owned();
        for i in 0..n {
            source.push_str(&format!("let x{i}: i64 = if bad {{ 1 }} else {{ 2 }};"));
        }
        source.push('}');
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert_eq!(checker.errors.len(), 1);
        assert_eq!(checker.expr_types.len(), 4 * n + 1);
        println!(
            "fanout={n} expression_records={} diagnostics={}",
            checker.expr_types.len(),
            checker.errors.len()
        );
    }
}

#[test]
fn sentinel_diagnostics_compound_type_shapes_and_work() {
    let error = TypeChecker::error_type();
    for ty in [
        error.clone(),
        Type::Array(Box::new(error.clone())),
        Type::Generic("Pair".into(), vec![Type::I64, error.clone()]),
        Type::Generic("Pair".into(), vec![error.clone(), Type::I64]),
        Type::Generic("Box".into(), vec![Type::Array(Box::new(error.clone()))]),
        Type::Fn(vec![error.clone()], Box::new(Type::I64)),
        Type::Fn(vec![Type::I64], Box::new(error.clone())),
        Type::Closure(vec![error.clone()], Box::new(Type::I64)),
        Type::Closure(vec![Type::I64], Box::new(error.clone())),
    ] {
        assert!(TypeChecker::contains_error_type(&ty), "{ty:?}");
    }
    for size in [1, 16, 256, 4096] {
        let mut deep = error.clone();
        for _ in 0..size {
            deep = Type::Array(Box::new(deep));
        }
        let mut args = vec![Type::I64; size];
        args.push(error.clone());
        let wide = Type::Generic("Tuple".into(), args);
        let clean = Type::Generic("Tuple".into(), vec![Type::I64; size]);
        for (shape, ty, found, visits) in [
            ("deep", &deep, true, size + 1),
            ("wide-last-error", &wide, true, size + 2),
            ("wide-clean", &clean, false, size + 1),
        ] {
            for uses in [1, 8] {
                RECOVERY_TYPE_VISITS.with(|count| count.set(0));
                for _ in 0..uses {
                    assert_eq!(TypeChecker::contains_error_type(ty), found);
                }
                let actual = RECOVERY_TYPE_VISITS.with(|count| count.get());
                assert_eq!(actual, visits * uses);
                println!("shape={shape} size={size} uses={uses} visits={actual}");
            }
        }
    }
    // Recovery detection must not weaken structural compatibility of siblings.
    let checker = TypeChecker::new();
    assert!(!checker.types_compatible(
        &Type::Generic("Pair".into(), vec![Type::Bool, error]),
        &Type::Generic("Pair".into(), vec![Type::I64, Type::String]),
    ));
}

#[test]
fn sentinel_diagnostics_partial_mismatch_display_and_compatibility() {
    let checker = TypeChecker::new();
    let bad = Type::Array(Box::new(TypeChecker::error_type()));
    assert!(checker.types_compatible(&Type::I64, &bad));
    // The recovered first component must not hide the independent second one.
    let actual = Type::Generic("Pair".into(), vec![bad, Type::Bool]);
    let expected = Type::Generic("Pair".into(), vec![Type::I64, Type::I64]);
    assert!(!checker.types_compatible(&expected, &actual));
    let display = type_name(&actual);
    assert_eq!(display, "Pair<Array<<error>>, bool>");
    assert!(!display.contains('\0') && !display.contains("diagnostic-error"));
    for ty in [
        Type::Fn(vec![actual.clone()], Box::new(TypeChecker::error_type())),
        Type::Closure(vec![TypeChecker::error_type()], Box::new(actual)),
    ] {
        let display = type_name(&ty);
        assert!(!display.contains('\0') && !display.contains("diagnostic-error"));
    }
}

#[test]
fn sentinel_diagnostics_mismatch_subtree_work() {
    let checker = TypeChecker::new();
    for width in [1, 16, 64] {
        for depth in [1, 16, 64] {
            let mut element = TypeChecker::error_type();
            for _ in 0..depth {
                element = Type::Array(Box::new(element));
            }
            let actual = Type::Generic("Tuple".into(), vec![element; width]);
            let expected = Type::Generic("Tuple".into(), vec![Type::I64; width]);
            RECOVERY_TYPE_VISITS.with(|count| count.set(0));
            assert!(checker.types_compatible(&expected, &actual));
            let visits = RECOVERY_TYPE_VISITS.with(|count| count.get());
            assert_eq!(visits, width * (depth + 2));
            println!("width={width} depth={depth} recovery_visits={visits}");
        }
    }
}

#[test]
fn sentinel_diagnostics_index_assignment_work() {
    for uses in [1, 16, 64, 256] {
        let mut source = "fn main() { let bad = absent; let xs = [1];".to_owned();
        for _ in 0..uses {
            source.push_str("xs[[bad]] = 2;");
        }
        source.push('}');
        RECOVERY_TYPE_VISITS.with(|count| count.set(0));
        let errors = check_source(&source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let visits = RECOVERY_TYPE_VISITS.with(|count| count.get());
        assert_eq!(visits, 2 * uses);
        println!("index_assignments={uses} recovery_visits={visits} diagnostics=1");
    }
}

#[test]
fn sentinel_diagnostics_try_known_shapes() {
    let error = TypeChecker::error_type();
    for (operand, return_ty, code) in [
        (
            Type::Generic("Option".into(), vec![error.clone()]),
            Type::Void,
            Some(ErrorCode::E1807),
        ),
        (
            Type::Generic("Option".into(), vec![error.clone()]),
            Type::Generic("Option".into(), vec![Type::I64]),
            None,
        ),
        (
            Type::Generic("Result".into(), vec![error.clone(), Type::I64]),
            Type::Void,
            Some(ErrorCode::E1807),
        ),
        (
            Type::Generic("Result".into(), vec![error.clone(), Type::I64]),
            Type::Generic("Result".into(), vec![Type::I64, Type::String]),
            Some(ErrorCode::E1805),
        ),
        (
            Type::Generic("Result".into(), vec![Type::I64, error.clone()]),
            Type::Generic("Result".into(), vec![Type::I64, Type::String]),
            None,
        ),
        (Type::Array(Box::new(error)), Type::Void, None),
    ] {
        let mut checker = TypeChecker::new();
        checker.symbols.push_scope();
        checker.local.current_return_type = return_ty;
        checker.define_var(
            "operand".into(),
            crate::semantic::symbols::VarInfo {
                ty: operand,
                mutable: false,
                is_param: false,
                declaration_span: Span::dummy(),
            },
        );
        checker.check_try_propagate(
            &Expr::Var("operand".into(), Span::dummy(), ExprId::fresh()),
            Span::dummy(),
        );
        assert_eq!(
            checker.errors.iter().map(|d| d.code).collect::<Vec<_>>(),
            code.into_iter().collect::<Vec<_>>()
        );
    }
}

#[test]
fn sentinel_diagnostics_range_try_work() {
    for uses in [1, 16, 64, 256] {
        for (expression, expected_visits) in [("[bad]..[bad]", 4), ("[bad]?", 2)] {
            let mut source = "fn main() { let bad = absent;".to_owned();
            for i in 0..uses {
                source.push_str(&format!("let x{i} = {expression};"));
            }
            source.push('}');
            RECOVERY_TYPE_VISITS.with(|count| count.set(0));
            let errors = check_source(&source);
            assert_eq!(errors.len(), 1, "{errors:?}");
            let visits = RECOVERY_TYPE_VISITS.with(|count| count.get());
            assert_eq!(visits, expected_visits * uses);
            println!("expression={expression} uses={uses} recovery_visits={visits}");
        }
    }
}

#[test]
fn sentinel_diagnostics_static_call_argument_work() {
    let marker = TypeChecker::error_type();
    let Type::Named(name) = &marker else {
        unreachable!()
    };
    for count in [0, 1, 16, 64, 256] {
        let args: Vec<_> = (0..count)
            .map(|i| CallArg::value(Expr::Integer(i, Span::dummy(), ExprId::fresh())))
            .collect();
        let mut checker = TypeChecker::new();
        let result = checker.resolve_static_call(
            name,
            &[],
            "make",
            &args,
            Span::dummy(),
            Span::dummy(),
            None,
        );
        assert!(TypeChecker::is_error_type(&result));
        assert!(checker.errors.is_empty());
        assert_eq!(checker.expr_types.len(), count as usize);
        println!(
            "static_recovery_arguments={count} recorded_expressions={}",
            checker.expr_types.len()
        );
    }
}

#[test]
fn sentinel_diagnostics_select_timeout_work() {
    for uses in [1, 16, 64, 256] {
        let mut source = "fn main() { let bad = absent;".to_owned();
        for _ in 0..uses {
            source.push_str("select { sleep([bad]) => {} }");
        }
        source.push('}');
        RECOVERY_TYPE_VISITS.with(|count| count.set(0));
        let errors = check_source(&source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let visits = RECOVERY_TYPE_VISITS.with(|count| count.get());
        assert_eq!(visits, 2 * uses);
        println!("select_timeouts={uses} recovery_visits={visits}");
    }
}

#[test]
fn sentinel_diagnostics_exact_builtin_contracts() {
    let checker = TypeChecker::new();
    assert!(!checker.recovery_types_compatible(&Type::I64, &Type::Bool));
    assert!(checker.recovery_types_compatible(
        &Type::I64,
        &Type::Array(Box::new(TypeChecker::error_type()))
    ));
    let expected = Type::Generic("Tuple".into(), vec![Type::I64, Type::I64]);
    for (known, accepted) in [(Type::I64, true), (Type::Bool, false)] {
        let actual = Type::Generic("Tuple".into(), vec![TypeChecker::error_type(), known]);
        assert_eq!(
            checker.recovery_types_compatible(&expected, &actual),
            accepted
        );
    }
}

#[test]
fn sentinel_diagnostics_lock_target_work() {
    for uses in [1, 16, 64, 256] {
        let mut source = "async fn main() { let bad = absent; let poison = [bad];".to_owned();
        for _ in 0..uses {
            source.push_str("lock poison as value {}");
        }
        source.push('}');
        RECOVERY_TYPE_VISITS.with(|count| count.set(0));
        let errors = check_source(&source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let visits = RECOVERY_TYPE_VISITS.with(|count| count.get());
        assert_eq!(visits, 2 * uses);
        println!("lock_targets={uses} recovery_visits={visits}");
    }
}

#[test]
fn parenthesized_callable_field_type_checks() {
    for (class, ty) in [("Plain", "i64"), ("Box<T>", "T")] {
        for callable in ["closure", "fn"] {
            for call in ["(self.f)(x)", "((self.f))(x)"] {
                let source = format!(
                    "class {class} {{ f: {callable}({ty}) -> {ty}; pub fn run(self, x: {ty}) -> {ty} {{ return {call}; }} }} fn main() {{}}"
                );
                let errors = check_source(&source);
                assert!(errors.is_empty(), "{source}: {errors:?}");
            }
        }
    }
    let inherited = "open class Base { pub f: closure(i64) -> i64; } class Child extends Base { pub fn run(self) -> i64 { return (self.f)(1); } } fn main() {}";
    assert!(check_source(inherited).is_empty());
    for source in [
        "class H { pub f: i64; } fn main() { let h = new H(1); (h.f)(2); }",
        "class H { pub f: closure(i64) -> i64; } fn main() { let h = new H(|x: i64| x); (h.f)(); }",
        "class H { pub f: closure(i64) -> i64; } fn main() { let h = new H(|x: i64| x); (h.f)(1, 2); }",
        "class H { pub f: closure(i64) -> i64; } fn main() { let h = new H(|x: i64| x); (h.f)(true); }",
        "class H { f: closure(i64) -> i64; } fn read(h: H) { (h.f)(1); } fn main() {}",
        "class H { pub f: closure(i64) -> i64; } fn read(h: H) { (h.absent)(1); } fn main() {}",
    ] {
        assert!(
            !check_source(source).is_empty(),
            "accepted invalid source: {source}"
        );
    }
}
