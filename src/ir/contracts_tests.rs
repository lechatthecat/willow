use super::{
    lower::{SemanticFacts, lower_program_with},
    lowered::{self, LirInst, LirRvalue},
};
use crate::{lexer::Lexer, parser::Parser, semantic::TypeChecker};

fn lowered(source: &str) -> lowered::LirProgram {
    let tokens = Lexer::new(source).tokenize().unwrap();
    let (program, errors) = Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    let (hir, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
    assert!(errors.is_empty(), "{errors:?}");
    lowered::lower_program(&hir)
}

#[test]
fn contracts_epilogue_code_growth_is_additive() {
    for exits in [1, 4, 16, 64] {
        for clauses in [1, 4, 16] {
            let mut source = "fn f(x:i64)->i64 ".to_owned();
            for i in 0..clauses {
                source.push_str(&format!("ensures result + {i} >= x "));
            }
            source.push('{');
            for i in 0..exits {
                source.push_str(&format!("if x == {i} {{ return x; }}"));
            }
            source.push_str("return x; } fn main() { println(f(0)); }");
            let lir = lowered(&source);
            let f = lir
                .functions
                .iter()
                .find(|f| f.name.is_free_named("f"))
                .unwrap();
            let count = |name| {
                f.blocks.iter().flat_map(|b| &b.instrs).filter(|i| matches!(i,
                LirInst::Compute { value: LirRvalue::BuiltinCall { callee, .. }, .. } if callee.is_free_named(name))).count()
            };
            assert_eq!(count("$contract_check"), clauses);
            assert_eq!(count("$contract_i64"), clauses);
            let instructions: usize = f.blocks.iter().map(|b| b.instrs.len()).sum();
            assert!(
                instructions <= 12 * (exits + clauses) + 20,
                "{instructions}"
            );
            println!(
                "exits={exits} clauses={clauses} checks={} checked_ops={} instructions={instructions}",
                count("$contract_check"),
                count("$contract_i64")
            );
        }
    }
}

#[test]
fn contracts_absent_do_not_add_calls_or_rename_result() {
    let lir = lowered(
        "fn f(x:i64)->i64 { let result = x + 1; return result; } fn main() { println(f(2)); }",
    );
    let f = lir
        .functions
        .iter()
        .find(|f| f.name.is_free_named("f"))
        .unwrap();
    assert!(f.locals.iter().any(|local| local.name == "result"));
    assert!(f.blocks.iter().flat_map(|b| &b.instrs).all(|i| !matches!(i,
        LirInst::Compute { value: LirRvalue::BuiltinCall { callee, .. }, .. }
        if callee.is_free_named("$contract_check") || callee.is_free_named("$contract_i64"))));
}

#[test]
fn contracts_deep_arithmetic_has_one_runtime_operation_per_node() {
    for depth in [16, 64, 256] {
        let source = format!(
            "fn f(x:i64)->i64 requires x{} > 0 {{ return x; }} fn main() {{ println(f(1)); }}",
            " + 1".repeat(depth)
        );
        let lir = lowered(&source);
        let f = lir
            .functions
            .iter()
            .find(|f| f.name.is_free_named("f"))
            .unwrap();
        let operations = f.blocks.iter().flat_map(|b| &b.instrs).filter(|i| matches!(i,
            LirInst::Compute { value: LirRvalue::BuiltinCall { callee, .. }, .. } if callee.is_free_named("$contract_i64"))).count();
        assert_eq!(operations, depth);
        println!("arithmetic_depth={depth} checked_ops={operations}");
    }
}

#[test]
fn inferred_loop_annotations_add_no_runtime_instructions() {
    for (parameters, iterable, annotations) in [
        ("", "0..3", "invariant i>=0 && i<=3 variant 3-i"),
        (
            "a:FrozenArray<i64>",
            "a",
            "invariant iteration>=0 && iteration<=a.len() variant a.len()-iteration",
        ),
    ] {
        let mut counts = Vec::new();
        for (attributes, clauses) in [
            ("@pure", ""),
            ("@pure @verified", ""),
            ("@pure @verified", annotations),
        ] {
            let source = format!(
                "{attributes} fn f({parameters})->i64 {{for i in {iterable} {clauses} {{}}return 0;}} fn main() {{}}"
            );
            let lir = lowered(&source);
            let f = lir
                .functions
                .iter()
                .find(|f| f.name.is_free_named("f"))
                .unwrap();
            let instructions: usize = f.blocks.iter().map(|b| b.instrs.len()).sum();
            let calls = f
                .blocks
                .iter()
                .flat_map(|b| &b.instrs)
                .filter(|i| {
                    matches!(i,
                LirInst::Compute { value: LirRvalue::BuiltinCall { callee, .. }, .. }
                if callee.is_free_named("$contract_check") || callee.is_free_named("$contract_i64"))
                })
                .count();
            assert_eq!(calls, 0);
            counts.push((f.blocks.len(), instructions, f.locals.len()));
        }
        assert_eq!(counts[0], counts[1]);
        assert_eq!(counts[1], counts[2]);
        eprintln!(
            "inferred runtime iterable={iterable} blocks/instructions/locals={:?}",
            counts[0]
        );
    }
}
