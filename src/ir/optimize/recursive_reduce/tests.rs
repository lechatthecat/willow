use super::*;

fn lowered(source: &str, checks: bool) -> crate::ir::lowered::SourceProgram {
    let source = format!("enum Chain {{ Link(i64, Chain), End }}\n{source}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
    let (ast, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let (hir, errors) = crate::ir::lower::lower_program(&ast);
    assert!(errors.is_empty(), "{errors:?}");
    crate::ir::lowered::lower_source_program_with(&hir, checks.into())
}

fn recursive_calls(f: &SourceFunction) -> usize {
    f.blocks.iter().flat_map(|b| &b.instrs).filter(|inst| matches!(inst,
        SourceInst::Compute { value: LirRvalue::DirectCall { callee, .. }, .. } if *callee == f.name)).count()
}

#[test]
fn enum_reduction_replaces_only_proven_wrapping_sum() {
    for checks in [false, true] {
        let program = lowered(
            "fn sum(c: Chain) -> i64 { return match c { Chain::Link(v, rest) => v + sum(rest), Chain::End => 0 }; }",
            checks,
        );
        assert_eq!(recursive_calls(&program.functions[0]), usize::from(checks));
    }
}

#[test]
fn enum_reduction_proof_covers_return_and_effect_boundaries() {
    // The positive cases exercise both operand orders, shared joins, explicit
    // returns, base values and parameter permutations. Negative cases keep
    // effects, arithmetic order, fault frames and multiple recursive calls.
    for (name, source, expected) in [
        (
            "right",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => f(r) + v, Chain::End => 9 }; }",
            0,
        ),
        (
            "returns",
            "fn f(c: Chain) -> i64 { match c { Chain::Link(v, r) => { return v + f(r); }, Chain::End => { return 5; } } }",
            0,
        ),
        (
            "parallel",
            "fn f(c: Chain, a: i64, b: i64) -> i64 { return match c { Chain::Link(v, r) => a + f(r, b, a), Chain::End => b }; }",
            0,
        ),
        (
            "copy",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => { let x = v + f(r); return x; }, Chain::End => 0 }; }",
            0,
        ),
        (
            "negative",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => -v + f(r), Chain::End => -7 }; }",
            0,
        ),
        (
            "branch",
            "fn f(c: Chain, stop: bool) -> i64 { if stop { return 13; } return match c { Chain::Link(v, r) => v + f(r, v == 0), Chain::End => 0 }; }",
            0,
        ),
        (
            "before",
            "fn f(c: Chain) -> i64 { println(1); return match c { Chain::Link(v, r) => v + f(r), Chain::End => 0 }; }",
            1,
        ),
        (
            "after",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => { let n = f(r); println(v); return v + n; }, Chain::End => 0 }; }",
            1,
        ),
        (
            "defer",
            "fn f(c: Chain) -> i64 { defer println(1); return match c { Chain::Link(v, r) => v + f(r), Chain::End => 0 }; }",
            1,
        ),
        (
            "subtraction",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => v - f(r), Chain::End => 0 }; }",
            1,
        ),
        (
            "multiply",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => v * f(r), Chain::End => 1 }; }",
            1,
        ),
        (
            "two_calls",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => f(r) + f(r), Chain::End => 0 }; }",
            2,
        ),
        (
            "result_twice",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => { let n = f(r); return n + n; }, Chain::End => 0 }; }",
            1,
        ),
        (
            "divide",
            "fn f(c: Chain) -> i64 { return match c { Chain::Link(v, r) => (10 / v) + f(r), Chain::End => 0 }; }",
            1,
        ),
        (
            "float",
            "fn f(c: Chain) -> f64 { return match c { Chain::Link(v, r) => 0.5 + f(r), Chain::End => 0.0 }; }",
            1,
        ),
        (
            "loop",
            "fn f(c: Chain, n: i64) -> i64 { let mut i = 0; while i < n { i = i + 1; } return match c { Chain::Link(v, r) => i + f(r, n), Chain::End => 0 }; }",
            1,
        ),
        (
            "reference",
            "fn f(c: Chain, n: &i64) -> i64 { return match c { Chain::Link(v, r) => n + f(r, &n), Chain::End => 0 }; }",
            1,
        ),
        (
            "collection",
            "fn f(c: Chain) -> i64 { gc_minor_collect(); return match c { Chain::Link(v, r) => v + f(r), Chain::End => 0 }; }",
            1,
        ),
    ] {
        let program = lowered(source, false);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.is_free_named("f"))
            .unwrap();
        assert_eq!(recursive_calls(f), expected, "{name}: {:#?}", f.blocks);
    }
}

#[test]
fn enum_reduction_generated_work_is_linear_for_large_prefixes_and_shared_joins() {
    for count in [4, 16, 64, 256] {
        let prefix = (0..count)
            .map(|i| format!("if n == {i} {{ return n; }}\n"))
            .collect::<String>();
        let program = lowered(
            &format!(
                "fn f(c: Chain, n: i64) -> i64 {{ {prefix} return match c {{ Chain::Link(v, r) => v + f(r, n), Chain::End => 0 }}; }}"
            ),
            false,
        );
        let f = &program.functions[0];
        assert_eq!(recursive_calls(f), 0);
        let instructions: usize = f.blocks.iter().map(|b| b.instrs.len()).sum();
        println!(
            "prefixes={count} blocks={} instructions={instructions} locals={}",
            f.blocks.len(),
            f.locals.len()
        );
        assert!(
            f.blocks.len() <= 4 * count + 12,
            "blocks={}",
            f.blocks.len()
        );
        assert!(
            instructions <= 16 * count + 40,
            "instructions={instructions}"
        );
        // One backedge, no copied traversal bodies, one parameter-copy group.
        let backedges = f.blocks.iter().filter(|b| matches!(b.terminator, SourceTerminator::Jump(target) if target.0 == f.blocks.len() - 1)).count();
        assert_eq!(backedges, 2); // one-time entry plus recursive step
    }
}
