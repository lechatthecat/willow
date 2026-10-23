use super::*;
use crate::{lexer::Lexer, parser::Parser, semantic::TypeChecker};

fn checked(source: &str) -> (Program, TypeChecker) {
    let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}: {source}");
    let mut checker = TypeChecker::new();
    crate::register_prelude(&mut checker).unwrap();
    checker.check_program(&program);
    assert!(checker.errors.is_empty(), "{:?}: {source}", checker.errors);
    (program, checker)
}

#[test]
fn semantic_facts_twenty_perspectives() {
    let cases = [
        "fn f() -> i64 { return 1; }",
        "fn f() -> f64 { return 1.5; }",
        "fn f() -> bool { return true; }",
        "fn f() -> String { return \"x\"; }",
        "fn f(x: i64) -> i64 { return x; }",
        "fn f() -> i64 { return (1 + 2) * 3; }",
        "fn f(x: i64) -> i64 { return -x; }",
        "fn f(x: bool) -> bool { return !x; }",
        "fn f() -> bool { return 1 < 2; }",
        "fn f() -> bool { return true && false; }",
        "fn f() -> i64 { let a = [1, 2]; return a[0]; }",
        "fn f() -> i64 { let a = 1; return a; }",
        "fn f() -> i64 { if true { return 1; } return 2; }",
        "fn f() { let mut a = 0; while a < 2 { a = a + 1; } }",
        "fn f() { for a in [1, 2] { println(a); } }",
        "fn f() { for a in 0..2 { println(a); } }",
        "fn g(x: i64) -> i64 { return x; } fn f() -> i64 { return g(1); }",
        "fn f() { let a = |x: i64| x + 1; println(a(2)); }",
        "enum E { A, B(i64) } fn f() -> E { return E::B(1); }",
        "enum E { A, B(i64) } fn f() -> i64 { return match E::B(1) { A => 0, B(x) => x }; }",
        "class C { pub x: i64; } fn f() -> i64 { let c = new C(1); return c.x; }",
        "class C { pub static fn value() -> i64 { return 1; } } fn f() -> i64 { return C::value(); }",
    ];
    for source in cases {
        let (program, checker) = checked(source);
        let (_, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
        assert!(errors.is_empty(), "{source}: {errors:?}");
        let empty = HashMap::new();
        let mut facts = SemanticFacts::from_checker(&checker);
        facts.expr_types = &empty;
        let (_, errors) = lower_program_with(&program, &facts);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("missing required semantic fact")),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn semantic_facts_missing_nested_operator() {
    let (program, mut checker) = checked("fn f() -> i64 { return (1 + 2) * 3; }");
    let Item::Function(f) = &program.items[0] else {
        panic!()
    };
    use crate::parser::iter::{AstEvent, AstWalk};
    let ids: Vec<_> = AstWalk::new(AstEvent::Block(&f.body))
        .filter_map(|e| match e {
            AstEvent::Expr(e) => Some(e.id()),
            _ => None,
        })
        .collect();
    for id in ids {
        let ty = checker.expr_types.remove(&id).unwrap();
        let (_, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
        assert!(
            errors.iter().any(|e| e.message.contains("expression type")),
            "{id}: {errors:?}"
        );
        checker.expr_types.insert(id, ty);
    }
}

#[test]
fn semantic_facts_missing_capture_and_static_target() {
    for (source, missing) in [
        (
            "fn f() { let x = 1; let a = |y: i64| y + x; println(a(2)); }",
            "lambda captures",
        ),
        (
            "class C { pub static fn v() -> i64 { return 1; } } fn f() { println(C::v()); }",
            "static target",
        ),
    ] {
        let (program, mut checker) = checked(source);
        checker.lambda_captures.clear();
        checker.static_call_classes.clear();
        let (_, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
        assert!(
            errors.iter().any(|e| e.message.contains(missing)),
            "{errors:?}"
        );
    }
}

#[test]
fn semantic_facts_reparse_rejects_stale_facts() {
    let (old, checker) = checked("fn f() -> i64 { return 1; }");
    let (edited, _) = checked("fn f() -> i64 { return 2; }");
    assert!(
        lower_program_with(&old, &SemanticFacts::from_checker(&checker))
            .1
            .is_empty()
    );
    assert!(
        lower_program_with(&edited, &SemanticFacts::from_checker(&checker))
            .1
            .iter()
            .any(|e| e.message.contains("expression type"))
    );
}

#[test]
fn semantic_facts_scope_and_memory() {
    for n in [16, 64, 256] {
        let source: String = (0..n)
            .map(|i| format!("fn function_{i}() -> i64 {{ return {i} + 1; }}\n"))
            .collect();
        let (program, checker) = checked(&source);
        let facts = SemanticFacts::from_checker(&checker);
        assert!(std::ptr::eq(facts.expr_types, &checker.expr_types));
        assert_eq!(facts.expr_types.len(), n * 3);
        assert_eq!(
            facts
                .expr_types
                .values()
                .filter(|t| **t == Type::I64)
                .count(),
            n * 3
        );
        REQUIRED_TYPE_READS.with(|reads| reads.set(0));
        let (hir, errors) = lower_program_with(&program, &facts);
        assert_eq!(REQUIRED_TYPE_READS.with(|reads| reads.get()), 3 * n);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(hir.functions.len(), n);
        // These scalar facts have no heap payload. Capacity accounts for
        // occupied/reserved entry storage, excluding hash-table control bytes.
        let entries = facts.expr_types.capacity() * std::mem::size_of::<(ExprId, Type)>();
        println!(
            "functions={n} expr_facts={} entry_capacity={} entry_storage_bytes={entries} borrowed_facts_bytes={} hir_functions={} required_type_reads={}",
            facts.expr_types.len(),
            facts.expr_types.capacity(),
            std::mem::size_of_val(&facts),
            hir.functions.len(),
            REQUIRED_TYPE_READS.with(|reads| reads.get())
        );
    }
}

#[test]
fn semantic_facts_missing_match_exit() {
    let (program, mut checker) = checked(
        "fn f(x: bool) -> i64 { match x { true => { return 1; }, false => { return 2; } } }",
    );
    assert!(
        lower_program_with(&program, &SemanticFacts::from_checker(&checker))
            .1
            .is_empty()
    );
    checker.arm_leaves.clear();
    let (_, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
    assert!(
        errors.iter().any(|e| e.message.contains("match arm exit")),
        "{errors:?}"
    );
}

#[test]
fn semantic_facts_deep_operator_reads_are_linear() {
    for n in [16, 64, 256] {
        let source = format!("fn f() -> i64 {{ return 0{}; }}", " + 1".repeat(n));
        let (program, checker) = checked(&source);
        REQUIRED_TYPE_READS.with(|reads| reads.set(0));
        let (_, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
        assert!(errors.is_empty(), "{errors:?}");
        let reads = REQUIRED_TYPE_READS.with(|reads| reads.get());
        assert_eq!(checker.expr_types.len(), 2 * n + 1);
        assert_eq!(reads, 2 * n + 1);
        println!(
            "operator_depth={n} expr_facts={} required_type_reads={reads}",
            checker.expr_types.len()
        );
    }
}

#[test]
fn semantic_facts_repeated_static_targets_share_identity() {
    for n in [16, 64, 256] {
        let mut source = "class Counter { pub static fn value() -> i64 { return 1; } }".to_owned();
        source.extend(
            (0..n).map(|i| format!("fn function_{i}() -> i64 {{ return Counter::value(); }}")),
        );
        let (program, checker) = checked(&source);
        let facts = SemanticFacts::from_checker(&checker);
        assert_eq!(facts.static_call_classes.len(), n);
        let targets: HashSet<_> = facts.static_call_classes.values().copied().collect();
        assert_eq!(targets.len(), 1);
        assert!(lower_program_with(&program, &facts).1.is_empty());
        println!(
            "static_calls={n} distinct_targets={} target_entry_capacity={} target_entry_storage_bytes={}",
            targets.len(),
            facts.static_call_classes.capacity(),
            facts.static_call_classes.capacity()
                * std::mem::size_of::<(ExprId, crate::semantic::ids::TypeId)>()
        );
    }
}

#[test]
fn semantic_facts_reference_places_twenty_four_perspectives() {
    use crate::parser::iter::{AstEvent, AstWalk};
    for (ty, spelling, value) in [
        (Type::I64, "i64", "1"),
        (Type::Bool, "bool", "true"),
        (Type::String, "String", "\"value\""),
    ] {
        for mode in ["&", "&mut"] {
            let local_mut = if mode == "&mut" { "mut " } else { "" };
            let sources = [
                format!("fn f() {{ let {local_mut}v = {value}; consume(&v); }}"),
                format!("fn f(v: {mode} {spelling}) {{ consume(&v); }}"),
                format!(
                    "class Holder {{ pub value: {spelling}; }} fn f() {{ let h = new Holder({value}); consume(&h.value); }}"
                ),
                format!("fn f() {{ let a = [{value}]; consume(&a[0]); }}"),
            ];
            for caller in sources {
                let source = format!("fn consume(v: {mode} {spelling}) {{}} {caller}");
                let (program, mut checker) = checked(&source);
                let arg = program
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Function(f) if f.name == "f" => Some(f),
                        _ => None,
                    })
                    .flat_map(|f| AstWalk::new(AstEvent::Block(&f.body)))
                    .find_map(|event| match event {
                        AstEvent::Expr(Expr::Call(call)) if call.callee == "consume" => {
                            Some(&call.args[0].expr)
                        }
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(checker.expr_types.get(&arg.id()), Some(&ty), "{source}");
                let (hir, errors) =
                    lower_program_with(&program, &SemanticFacts::from_checker(&checker));
                assert!(errors.is_empty(), "{source}: {errors:?}");
                // Exercise the actual reference-place conversion, not only HIR.
                let _ = crate::ir::lowered::lower_source_program(&hir);
                checker.expr_types.remove(&arg.id());
                let (_, errors) =
                    lower_program_with(&program, &SemanticFacts::from_checker(&checker));
                assert!(
                    errors.iter().any(|error| error.code == ErrorCode::E0890),
                    "strict check lost: {source}: {errors:?}"
                );
            }
        }
    }
}

#[test]
fn semantic_facts_reference_place_storage_is_linear() {
    for n in [16, 64, 256] {
        let source = format!(
            "fn consume(v: &i64) {{}} fn f() {{ let v = 0; let a = [0]; {} }}",
            "consume(&v); consume(&a[0]);".repeat(n)
        );
        let (program, checker) = checked(&source);
        assert_eq!(checker.expr_types.len(), 6 * n + 3);
        assert!(
            lower_program_with(&program, &SemanticFacts::from_checker(&checker))
                .1
                .is_empty()
        );
        println!(
            "reference_arguments={} expr_facts={} entry_capacity={} entry_storage_bytes={}",
            2 * n,
            checker.expr_types.len(),
            checker.expr_types.capacity(),
            checker.expr_types.capacity() * std::mem::size_of::<(ExprId, Type)>()
        );
    }
}
