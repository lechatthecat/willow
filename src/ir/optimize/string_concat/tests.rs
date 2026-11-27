use super::*;
use crate::diagnostics::Span;
use crate::ir::lowered::{
    BlockId, LirLocal, LirPlace, LirStorageKind, SourceProgram, SourceTerminator,
};

fn lowered(source: &str) -> SourceProgram {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (ast, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).unwrap();
    checker.check_program(&ast);
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    let tables = crate::ir::lower::SemanticFacts::from_checker(&checker);
    let (hir, errors) = crate::ir::lower::lower_program_with(&ast, &tables);
    assert!(errors.is_empty(), "{errors:?}");
    crate::ir::lowered::lower_source_program(&hir)
}

fn function<'a>(program: &'a SourceProgram, name: &str) -> &'a SourceFunction {
    program
        .functions
        .iter()
        .find(|f| f.name.is_free_named(name))
        .unwrap()
}

/// Every Compute right-hand side of `function` and its defer bodies.
fn values(function: &SourceFunction) -> Vec<&LirRvalue> {
    let mut values = Vec::new();
    let mut functions = vec![function];
    while let Some(function) = functions.pop() {
        for instruction in function.blocks.iter().flat_map(|block| &block.instrs) {
            match instruction {
                SourceInst::Compute { value, .. } => values.push(value),
                SourceInst::Defer { body, .. } => functions.push(&body.function),
                _ => {}
            }
        }
    }
    values
}

/// Part types of each fused concatenation.
fn concats(function: &SourceFunction) -> Vec<Vec<Type>> {
    values(function)
        .into_iter()
        .filter_map(|value| match value {
            LirRvalue::StringConcat { part_types, .. } => Some(part_types.clone()),
            _ => None,
        })
        .collect()
}

fn string_adds(function: &SourceFunction) -> usize {
    values(function)
        .into_iter()
        .filter(|value| {
            matches!(
                value,
                LirRvalue::Binary {
                    op: BinOp::Add,
                    operand_ty: Type::String,
                    ..
                }
            )
        })
        .count()
}

/// The deferred operand must read a typed snapshot defined before the
/// writer. Merely producing one concat would also accept a late alias read.
fn assert_saved_string(f: &SourceFunction) {
    assert_eq!(string_adds(f), 0);
    assert_eq!(concats(f), vec![vec![S, S, S]]);
    let instrs: Vec<_> = f.blocks.iter().flat_map(|b| &b.instrs).collect();
    let (end, snapshot) = instrs
        .iter()
        .enumerate()
        .find_map(|(i, inst)| {
            let (_, LirRvalue::StringConcat { parts, .. }) = computation(inst)? else {
                return None;
            };
            let LirOperand::Local(id) = parts[0] else {
                return None;
            };
            Some((i, id))
        })
        .unwrap();
    let start = instrs[..end]
        .iter()
        .position(|inst| {
            matches!(computation(inst),
            Some((dest, LirRvalue::Use(LirOperand::Local(_)))) if dest == snapshot)
        })
        .unwrap();
    assert!(f.locals[snapshot.0 as usize].synthetic);
    assert!(instrs[start + 1..end].iter().any(|inst| matches!(
        computation(inst),
        Some((
            _,
            LirRvalue::DirectCall { .. }
                | LirRvalue::IndirectCall { .. }
                | LirRvalue::IntrinsicCall { .. }
        ))
    )));
}

fn i64_to_strings(function: &SourceFunction) -> usize {
    values(function)
        .into_iter()
        .filter(|value| {
            matches!(
                value,
                LirRvalue::IntrinsicCall {
                    intrinsic: Intrinsic::I64ToString,
                    ..
                }
            )
        })
        .count()
}

use crate::parser::ast::Type::{I64, String as S};

#[test]
fn left_nested_chain_becomes_one_concatenation() {
    let program = lowered(
        r#"fn f(a: String, b: String) -> String { return a + "-" + b + "!"; }
               fn main() { println(f("x", "y")); }"#,
    );
    let f = function(&program, "f");
    assert_eq!(concats(f), vec![vec![S, S, S, S]]);
    assert_eq!(string_adds(f), 0);
}

#[test]
fn two_string_operands_keep_the_pairwise_entry() {
    let program = lowered(
        r#"fn f(a: String, b: String) -> String { return a + b; }
               fn main() { println(f("x", "y")); }"#,
    );
    let f = function(&program, "f");
    assert!(concats(f).is_empty());
    assert_eq!(string_adds(f), 1);
}

#[test]
fn i64_to_string_operands_become_decimal_parts() {
    let program = lowered(
        r#"fn f(a: String, n: i64) -> String { return a + n.to_string(); }
               fn g(n: i64) -> String { return "k" + n.to_string() + ":" + (n * 7).to_string(); }
               fn main() { println(f("x", 1)); println(g(2)); }"#,
    );
    assert_eq!(concats(function(&program, "f")), vec![vec![S, I64]]);
    assert_eq!(concats(function(&program, "g")), vec![vec![S, I64, S, I64]]);
    assert_eq!(i64_to_strings(function(&program, "g")), 0);
}

#[test]
fn a_lone_to_string_is_left_alone() {
    let program = lowered(
        r#"fn f(n: i64) -> String { return n.to_string(); }
               fn main() { println(f(1)); }"#,
    );
    let f = function(&program, "f");
    assert!(concats(f).is_empty());
    assert_eq!(i64_to_strings(f), 1);
}

#[test]
fn right_and_balanced_nesting_flatten_in_source_order() {
    let program = lowered(
        r#"fn r(a: String, b: String, c: String) -> String { return a + (b + c); }
               fn q(a: String, b: String, c: String, n: i64) -> String {
                   return (a + n.to_string()) + (b + c);
               }
               fn main() { println(r("a", "b", "c")); println(q("a", "b", "c", 1)); }"#,
    );
    assert_eq!(concats(function(&program, "r")), vec![vec![S, S, S]]);
    assert_eq!(concats(function(&program, "q")), vec![vec![S, I64, S, S]]);
    let q = function(&program, "q");
    let fused = values(q)
        .into_iter()
        .find_map(|value| match value {
            LirRvalue::StringConcat { parts, .. } => Some(parts.clone()),
            _ => None,
        })
        .unwrap();
    // Parameters are locals 0..4 in declaration order: `a`, decimal `n`,
    // `b`, `c`.
    assert_eq!(fused[0], LirOperand::Local(LirLocalId(0)));
    assert_eq!(fused[2], LirOperand::Local(LirLocalId(1)));
    assert_eq!(fused[3], LirOperand::Local(LirLocalId(2)));
}

#[test]
fn format_fuses_integer_and_string_arguments() {
    let program = lowered(
        r#"fn f(name: String, n: i64) -> String { return format("{}:{}:{}!", name, n, 1.5); }
               fn main() { println(f("x", 1)); }"#,
    );
    let f = function(&program, "f");
    assert_eq!(concats(f), vec![vec![S, S, I64, S, S, S]]);
    // The f64 argument keeps its own formatting call.
    assert!(
        values(f)
            .iter()
            .any(|value| matches!(value, LirRvalue::FormatScalar { ty: Type::F64, .. }))
    );
}

#[test]
fn calls_between_operands_still_fuse() {
    let program = lowered(
        r#"fn p(s: String) -> String { println(s); return s; }
               fn f() -> String { return p("a") + p("b") + p("c"); }
               fn main() { println(f()); }"#,
    );
    assert_eq!(concats(function(&program, "f")), vec![vec![S, S, S]]);
}

#[test]
fn named_intermediates_are_not_absorbed() {
    let program = lowered(
        r#"fn f(a: String, b: String) -> String { let t = a + b; return t + a; }
               fn main() { println(f("x", "y")); }"#,
    );
    let f = function(&program, "f");
    assert!(concats(f).is_empty());
    assert_eq!(string_adds(f), 2);
}

#[test]
fn defer_and_lambda_bodies_fuse() {
    let program = lowered(
        r#"fn f(a: String, n: i64) {
                   defer println(a + "-" + n.to_string());
                   println("body");
               }
               fn main() {
                   let g = |s: String| s + "!" + "?";
                   println(g("x"));
                   f("y", 2);
               }"#,
    );
    assert_eq!(concats(function(&program, "f")), vec![vec![S, S, I64]]);
    let lambda = program
        .lambdas
        .iter()
        .find(|lambda| !concats(&lambda.function).is_empty())
        .expect("fused lambda body");
    assert_eq!(concats(&lambda.function), vec![vec![S, S, S]]);
}

// -- hand-built blocks ---------------------------------------------------

/// `sources` named String locals, then `temps` synthetic String temporaries.
fn hand_built(sources: u32, temps: u32, instrs: Vec<SourceInst>) -> SourceFunction {
    let mut function = lowered("fn main() {}")
        .functions
        .into_iter()
        .find(|f| f.name.is_free_named("main"))
        .unwrap();
    function.locals = (0..sources + temps)
        .map(|id| LirLocal {
            storage_kind: LirStorageKind::GcOwner,
            id: LirLocalId(id),
            name: format!("l{id}"),
            ty: Type::String,
            source_span: None,
            synthetic: id >= sources,
            parameter: false,
        })
        .collect();
    function.blocks = vec![SourceBlock {
        id: BlockId(0),
        instrs,
        terminator: SourceTerminator::Return(None),
        recovery: Vec::new(),
    }];
    function
}

fn local(id: u32) -> LirOperand {
    LirOperand::Local(LirLocalId(id))
}

fn compute(target: u32, value: LirRvalue) -> SourceInst {
    SourceInst::Compute {
        local: LirLocalId(target),
        value,
        span: Span::dummy(),
    }
}

fn add(target: u32, lhs: LirOperand, rhs: LirOperand) -> SourceInst {
    compute(
        target,
        LirRvalue::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
            operand_ty: Type::String,
        },
    )
}

fn fused(instrs: Vec<SourceInst>, sources: u32, temps: u32) -> SourceFunction {
    let mut function = hand_built(sources, temps, instrs);
    fuse_string_concats(&mut function);
    function
}

fn return_local(id: u32) -> SourceTerminator {
    use crate::ir::typed_ast::{HirExpr, HirExprKind};
    SourceTerminator::Return(Some(HirExpr {
        kind: HirExprKind::Var(format!("l{id}")),
        ty: Type::String,
        span: Span::dummy(),
    }))
}

#[test]
fn return_immediately_after_definition_counts_as_a_read() {
    let mut function = hand_built(2, 1, vec![add(2, local(0), local(1))]);
    function.blocks[0].terminator = return_local(2);
    let accesses = Accesses::count(&function);
    assert_eq!(accesses.reads, [1, 1, 1]);
    assert_eq!(accesses.writes, [0, 0, 1]);
}

#[test]
fn return_from_an_empty_block_counts_its_own_read() {
    let mut function = hand_built(2, 1, vec![add(2, local(0), local(1))]);
    function.blocks[0].terminator = SourceTerminator::Jump(BlockId(1));
    function.blocks.push(SourceBlock {
        id: BlockId(1),
        instrs: Vec::new(),
        terminator: return_local(2),
        recovery: Vec::new(),
    });
    let accesses = Accesses::count(&function);
    assert_eq!(accesses.reads, [1, 1, 1]);
    assert_eq!(accesses.writes, [0, 0, 1]);
}

#[test]
fn a_temporary_read_twice_is_not_absorbed() {
    let f = fused(
        vec![
            add(3, local(0), local(1)),
            add(4, local(3), local(2)),
            add(5, local(3), local(0)),
        ],
        3,
        3,
    );
    assert!(concats(&f).is_empty());
    assert_eq!(f.blocks[0].instrs.len(), 3);
}

#[test]
fn pending_operand_work_for_repeated_call_arguments() {
    let callee = FunctionId::free_from_source_name("consume");
    for arity in [1, 8, 64, 512, 4096] {
        for live in [false, true] {
            let mut instructions = Vec::new();
            let argument = if live {
                instructions.push(add(2, local(0), local(1)));
                local(2)
            } else {
                local(0)
            };
            instructions.push(compute(
                3,
                LirRvalue::DirectCall {
                    callee,
                    args: vec![argument; arity],
                    params: vec![S; arity],
                    result: S,
                },
            ));
            let f = hand_built(2, 2, instructions);
            let accesses = Accesses::count(&f);
            PENDING_OPERAND_VISITS.set(0);
            let nodes = plan_block(&f.blocks[0], &accesses, &f.locals, &HashSet::new());
            assert_eq!(nodes.len(), usize::from(live));
            // A live child is read at every position and retired just once.
            assert_eq!(
                PENDING_OPERAND_VISITS.get(),
                if live { arity + 1 } else { 0 }
            );
        }
    }
}

#[test]
fn wide_concat_counts_pending_children_in_linear_work() {
    for width in [4, 32, 256, 2048] {
        let sources = width + 1;
        let children = sources..sources + width;
        let mut instructions: Vec<_> = children
            .clone()
            .enumerate()
            .map(|(i, child)| add(child, local(i as u32), local(i as u32 + 1)))
            .collect();
        instructions.push(compute(
            sources + width,
            LirRvalue::StringConcat {
                parts: children.map(local).collect(),
                part_types: vec![S; width as usize],
            },
        ));
        let mut function = hand_built(sources, width + 1, instructions);
        PENDING_OPERAND_VISITS.with(|visits| visits.set(0));
        fuse_string_concats(&mut function);
        // The first two leaves have no pending children. Each subsequent
        // leaf is counted and checked, and each child is retired once.
        assert_eq!(
            PENDING_OPERAND_VISITS.with(|visits| visits.get()),
            7 * width as usize - 4
        );
        let [
            SourceInst::Compute {
                value: LirRvalue::StringConcat { parts, .. },
                ..
            },
        ] = function.blocks[0].instrs.as_slice()
        else {
            panic!("expected one fused concat")
        };
        let expected: Vec<_> = (0..width).flat_map(|i| [local(i), local(i + 1)]).collect();
        assert_eq!(parts, &expected);
    }
}

#[test]
fn a_double_read_in_one_instruction_is_not_absorbed() {
    let f = fused(
        vec![add(3, local(0), local(1)), add(4, local(3), local(3))],
        3,
        2,
    );
    assert!(concats(&f).is_empty());
}

#[test]
fn repeated_child_stays_materialized_when_its_sibling_fuses() {
    for repeats in [2, 257] {
        let mut parts = vec![local(2); repeats];
        parts.push(local(3));
        let f = fused(
            vec![
                add(2, local(0), local(1)),
                add(3, local(1), local(0)),
                compute(
                    4,
                    LirRvalue::StringConcat {
                        part_types: vec![S; parts.len()],
                        parts,
                    },
                ),
            ],
            2,
            3,
        );
        let [
            SourceInst::Compute { local: kept, .. },
            SourceInst::Compute {
                value: LirRvalue::StringConcat { parts, .. },
                ..
            },
        ] = f.blocks[0].instrs.as_slice()
        else {
            panic!("expected a retained child and a fused parent")
        };
        assert_eq!(*kept, LirLocalId(2));
        let mut expected = vec![local(2); repeats];
        expected.extend([local(1), local(0)]);
        assert_eq!(parts, &expected);
    }
}

#[test]
fn reference_writes_share_one_snapshot_of_the_same_value() {
    for width in [4, 32, 256] {
        // Half the children read l0 before the same write. Its saved
        // pointer is shared. l1 and l2 can be read at the consumer.
        let children = 3..3 + 2 * width;
        let last = children.end;
        let mut instrs: Vec<_> = children
            .clone()
            .map(|id| add(id, local((id - 3) % 2), local(2)))
            .collect();
        let call = LirRvalue::DirectCall {
            callee: crate::semantic::ids::FunctionId::free_from_source_name("change"),
            args: vec![LirOperand::Reference {
                place: LirPlace::Local(LirLocalId(0)),
                span: Span::dummy(),
                display: "l0".into(),
            }],
            params: vec![S],
            result: S,
        };
        instrs.extend((0..width).map(|_| compute(last, call.clone())));
        instrs.push(compute(
            last + 1,
            LirRvalue::StringConcat {
                parts: children.map(local).collect(),
                part_types: vec![S; (2 * width) as usize],
            },
        ));
        let mut function = hand_built(3, 2 * width + 2, instrs);
        SNAPSHOT_LEAF_VISITS.set(0);
        fuse_string_concats(&mut function);
        assert_eq!(SNAPSHOT_LEAF_VISITS.get(), 4 * width as usize);
        assert_eq!(string_adds(&function), 0);
        let snapshot_id = last + 2;
        assert_eq!(function.locals.len(), snapshot_id as usize + 1);
        assert!(matches!(computation(&function.blocks[0].instrs[0]),
                Some((id, LirRvalue::Use(op))) if id.0 == snapshot_id && op == &local(0)));
        let Some(SourceInst::Compute {
            value: LirRvalue::StringConcat { parts, .. },
            ..
        }) = function.blocks[0].instrs.last()
        else {
            panic!("expected the fused parent")
        };
        let expected: Vec<_> = (0..width)
            .flat_map(|_| [local(snapshot_id), local(2), local(1), local(2)])
            .collect();
        assert_eq!(parts, &expected);
    }
}

#[test]
fn repeated_barriers_copy_each_part_once() {
    for groups in [1, 2, 4, 8, 16, 32] {
        let expression = vec![r#"x + ":" + helper(n)"#; groups].join(" + ");
        let source = format!(
            r#"
                import std::collections::Array;
                class Holder {{ pub items: Array<String>; }}
                fn grow(items:Array<String>, n:String) {{ items.push(n); }}
                fn helper(n:String)->String {{
                    let box = new Holder([n,n]);
                    if n.len() == 8 {{return n;}}
                    let items = box.items;
                    grow(items,n);
                    if items.len() == 0 {{return "empty";}}
                    return n;
                }}
                fn render(x:&String,n:String)->String {{return {expression};}}
            "#
        );
        let program = lowered(&source);
        let render = function(&program, "render");
        assert_eq!(string_adds(render), 0);
        assert_eq!(concats(render), vec![vec![S; 3 * groups]]);
        let mut saved = Vec::new();
        let mut calls = 0;
        for inst in render.blocks.iter().flat_map(|b| &b.instrs) {
            match computation(inst) {
                Some((id, LirRvalue::Use(op))) if op == &local(0) => {
                    assert_eq!(saved.len(), calls);
                    saved.push(id);
                }
                Some((_, LirRvalue::DirectCall { callee, .. }))
                    if callee.is_free_named("helper") =>
                {
                    calls += 1;
                    assert_eq!(saved.len(), calls);
                }
                Some((_, LirRvalue::StringConcat { parts, .. })) => {
                    assert_eq!(calls, groups);
                    for (part, &id) in parts.chunks_exact(3).zip(&saved) {
                        assert_eq!(part[0], LirOperand::Local(id));
                    }
                }
                _ => {}
            }
        }
        assert_eq!(calls, groups);
    }
}

#[test]
fn borrowed_string_stays_fused_across_read_only_direct_calls() {
    let program = lowered(
        r#"
            fn helper(n: i64) -> String {
                if n == 0 { return "zero"; }
                return n.to_string();
            }
            fn wrapper(n: i64) -> String { return helper(n); }
            fn read(x: &String) -> String { return x; }
            fn render(x: &String, n: i64) -> String { return x + ":" + wrapper(n); }
            fn local() -> String { let x = "old"; return x + ":" + read(&x); }
            fn change(x: &mut String) -> String { x = "new"; return "!"; }
            fn forward(x: &mut String) -> String { return change(&x); }
            fn write(x: &mut String) -> String { return x + ":" + forward(&x); }
        "#,
    );
    let render = function(&program, "render");
    assert!(values(render).iter().any(
        |v| matches!(v, LirRvalue::DirectCall { callee, .. } if callee.is_free_named("wrapper"))
    ));
    assert_eq!(concats(render), vec![vec![S, S, S]]);
    assert_eq!(concats(function(&program, "local")), vec![vec![S, S, S]]);
    assert_saved_string(function(&program, "write"));
}

#[test]
fn borrowed_string_stays_fused_across_string_length_call() {
    let program = lowered(
        r#"
            fn helper(n: String) -> String {
                if n.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
        "#,
    );
    assert!(
        values(function(&program, "helper"))
            .iter()
            .any(|v| matches!(
                v,
                LirRvalue::IntrinsicCall {
                    intrinsic: Intrinsic::StringLen,
                    ..
                }
            ))
    );
    let render = function(&program, "render");
    assert!(values(render).iter().any(
        |v| matches!(v, LirRvalue::DirectCall { callee, .. } if callee.is_free_named("helper"))
    ));
    assert_eq!(concats(render), vec![vec![S, S, S]]);
    assert_eq!(string_adds(render), 0);
}

#[test]
fn fresh_aggregate_stores_allow_fusion_but_stores_to_inputs_do_not() {
    let program = lowered(
        r#"
            import std::collections::Array;
            class Cell { pub text: String; }
            enum Pair { Values(String, String) }
            fn helper(n: String) -> String {
                let items = [n, n];
                if items.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
            fn fresh_object(n: String) -> String {
                let cell = new Cell(n);
                return cell.text;
            }
            fn fresh_enum(n: String) -> Pair { return Pair::Values(n, n); }
            fn replace(items: Array<String>) -> String { items[0] = "new"; return "!"; }
            fn write(x: &String, items: Array<String>) -> String {
                return x + ":" + replace(items);
            }
        "#,
    );
    let helper = function(&program, "helper");
    assert_eq!(
        values(helper)
            .iter()
            .filter(|v| matches!(v, LirRvalue::ArrayStore { .. }))
            .count(),
        2
    );
    let render = function(&program, "render");
    assert!(
        values(render)
            .iter()
            .any(|v| matches!(v, LirRvalue::DirectCall { callee, .. } if *callee == helper.name))
    );
    assert_eq!(concats(render), vec![vec![S, S, S]]);
    assert_eq!(string_adds(render), 0);
    for name in ["fresh_object", "fresh_enum"] {
        assert!(!body_may_write(function(&program, name)), "{name}");
    }
    assert_saved_string(function(&program, "write"));
}

#[test]
fn stores_follow_named_allocations_across_branches_joins_and_loops() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn helper(n: String) -> String {
                let items = [n, n];
                if n.len() == 8 { return n; }
                items[0] = n;
                if items.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
            fn join(flag: bool, n: String, input: Array<String>) -> String {
                let mut items = input;
                if flag { items = [n]; } else { items = [n,n]; }
                items[0] = n;
                return n;
            }
            fn mixed(flag: bool, n: String, input: Array<String>) -> String {
                let mut items = [n];
                if flag { items = input; }
                items[0] = n;
                return n;
            }
            fn loop_fresh(limit: i64, n: String) -> String {
                let mut items = [n];
                let mut i = 0;
                while i < limit { items[0] = n; i = i + 1; }
                return n;
            }
            fn loop_alias(limit: i64, n: String, input: Array<String>) -> String {
                let mut items = [n];
                let mut i = 0;
                while i < limit { items[0] = n; items = input; i = i + 1; }
                return n;
            }
        "#,
    );
    let render = function(&program, "render");
    assert!(values(render).iter().any(
        |v| matches!(v, LirRvalue::DirectCall { callee, .. } if callee.is_free_named("helper"))
    ));
    assert_eq!(concats(render), vec![vec![S, S, S]]);
    for name in ["helper", "join", "loop_fresh"] {
        assert!(!body_may_write(function(&program, name)), "{name}");
    }
    for name in ["mixed", "loop_alias"] {
        assert!(body_may_write(function(&program, name)), "{name}");
    }
}

#[test]
fn direct_call_results_prove_freshness_on_every_return_path() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn make(n: String) -> Array<String> {
                if n.len() == 0 { return [n]; }
                return [n,n];
            }
            fn wrap(n: String) -> Array<String> { return make(n); }
            fn mixed(n: String, input: Array<String>) -> Array<String> {
                if n.len() == 8 { return input; }
                return make(n);
            }
            fn wrap_mixed(n: String, input: Array<String>) -> Array<String> {
                return mixed(n, input);
            }
            fn push_new(n: String) -> String {
                let items = wrap(n);
                if n.len() == 8 { return n; }
                items.push(n);
                return n;
            }
            fn push_mixed(n: String, input: Array<String>) -> String {
                let items = wrap_mixed(n,input);
                items.push(n);
                return n;
            }
            fn fresh(x: &String,n: String) -> String {return x+":"+push_new(n);}
            fn borrowed(x: &String,n: String,a: Array<String>) -> String {return x+":"+push_mixed(n,a);}
        "#,
    );
    let direct = program.functions.iter().map(|f| f.name).collect();
    let callees = summarize_callees(&program.functions, direct);
    for name in ["make", "wrap", "mixed", "wrap_mixed"] {
        let out = callees.functions[&function(&program, name).name];
        assert!(callees.graph.facts[out.writes].is_empty());
        let expected = if name.contains("mixed") {
            HashSet::from([Fact::Value(1)])
        } else {
            HashSet::new()
        };
        assert_eq!(callees.graph.facts[out.result], expected, "{name}");
    }
    assert_eq!(concats(function(&program, "fresh")), vec![vec![S, S, S]]);
    assert_saved_string(function(&program, "borrowed"));
}

#[test]
fn receiver_mutations_use_the_array_origin_across_a_branch() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn helper(n: String) -> String {
                let items = [n, n];
                if n.len() == 8 { return n; }
                items.push(n);
                if items.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
            fn grow(items: Array<String>, n: String) -> String { items.push(n); return n; }
            fn write(x: &String, items: Array<String>, n: String) -> String {
                return x + ":" + grow(items, n);
            }
        "#,
    );
    for (name, writes) in [("helper", false), ("grow", true)] {
        let f = function(&program, name);
        let (resize, receiver) = values(f)
            .into_iter()
            .find_map(|v| {
                if let LirRvalue::IntrinsicCall {
                    intrinsic: Intrinsic::ArrayResize,
                    receiver: LirOperand::Local(id),
                    ..
                } = v
                {
                    Some((v, *id))
                } else {
                    None
                }
            })
            .expect("push must resize the array receiver");
        // Check resize independently of the element store also emitted by push.
        for origin in [Origin::Fresh, Origin::Unknown, Origin::Incoming(receiver)] {
            let mut state = BlockOrigins::default();
            state.set(receiver, origin);
            assert!(
                may_write_aliased_storage(
                    LirLocalId(receiver.0 + 1),
                    resize,
                    &mut state,
                    &mut FieldOrigins::default(),
                    |_, _, _, _| CallTransfer::Opaque,
                ) == origin
            );
        }
        assert_eq!(body_may_write(f), writes, "{name}");
    }
    assert_eq!(concats(function(&program, "render")), vec![vec![S, S, S]]);
    assert_saved_string(function(&program, "write"));
}

/// A graph with an allocation per local at entry and a store per local
/// in the exit block. Only the intervening control flow varies.
fn array_stores_after(locals: usize, terms: Vec<SourceTerminator>) -> SourceFunction {
    let ids = 0..locals as u32;
    let allocations = ids
        .clone()
        .map(|id| {
            compute(
                id,
                LirRvalue::ArrayAlloc {
                    length: 1,
                    element: Type::I64,
                },
            )
        })
        .collect();
    let mut f = hand_built(0, locals as u32 + 1, allocations);
    for array in &mut f.locals[..locals] {
        array.ty = Type::Array(Box::new(Type::I64));
    }
    f.locals[locals].ty = Type::Void;
    f.locals[locals].storage_kind = LirStorageKind::Value;
    for (b, term) in terms.into_iter().enumerate() {
        f.blocks[b].terminator = term;
        f.blocks.push(SourceBlock {
            id: BlockId(b + 1),
            instrs: vec![],
            recovery: vec![],
            terminator: SourceTerminator::Return(None),
        });
    }
    f.blocks.last_mut().unwrap().instrs.extend(ids.map(|id| {
        compute(
            locals as u32,
            LirRvalue::ArrayStore {
                array: local(id),
                index: LirOperand::Int(0),
                value: LirOperand::Int(1),
                element: Type::I64,
            },
        )
    }));
    f
}

#[test]
fn independent_origins_share_unchanged_blocks() {
    for (blocks, locals) in [(8, 8), (64, 8), (8, 64), (64, 64), (512, 512), (2048, 2048)] {
        let f = array_stores_after(
            locals,
            (1..=blocks + 1)
                .map(|id| SourceTerminator::Jump(BlockId(id)))
                .collect(),
        );
        ORIGIN_EDGE_VISITS.set(0);
        assert!(!body_may_write(&f));
        assert_eq!(ORIGIN_EDGE_VISITS.get(), blocks + locals);
        assert_eq!(ORIGIN_RETAINED.get(), (blocks + 2, 1, locals, locals, 0));
    }
}

#[test]
fn diamonds_share_agreeing_origins_for_independent_stores() {
    use crate::ir::typed_ast::{HirExpr, HirExprKind};
    for (diamonds, locals) in [(8, 8), (64, 8), (8, 64), (64, 64), (512, 512), (2048, 2048)] {
        let terms = (0..diamonds)
            .flat_map(|d| {
                let b = 3 * d;
                [
                    SourceTerminator::Branch {
                        cond: HirExpr {
                            kind: HirExprKind::Bool(true),
                            ty: Type::Bool,
                            span: Span::dummy(),
                        },
                        then_block: BlockId(b + 1),
                        else_block: BlockId(b + 2),
                    },
                    SourceTerminator::Jump(BlockId(b + 3)),
                    SourceTerminator::Jump(BlockId(b + 3)),
                ]
            })
            .collect();
        let f = array_stores_after(locals, terms);
        ORIGIN_EDGE_VISITS.set(0);
        assert!(!body_may_write(&f));
        assert_eq!(ORIGIN_EDGE_VISITS.get(), 4 * diamonds);
        assert_eq!(
            ORIGIN_RETAINED.get(),
            (3 * diamonds + 1, 1, locals, locals, 0)
        );
    }
}

#[test]
fn dependency_discovery_deduplicates_repeated_arguments_and_stores() {
    let factory = FunctionId::free_from_source_name("factory");
    let consumer = FunctionId::free_from_source_name("consumer");
    let prefix_call = FunctionId::free_from_source_name("prefix_call");
    let array = Type::Array(Box::new(I64));
    for (arity, stores) in [
        (1, 0),
        (0, 1),
        (1, 1),
        (8, 1),
        (64, 1),
        (512, 1),
        (1, 512),
        (512, 512),
    ] {
        let mut f = array_stores_after(1, vec![SourceTerminator::Jump(BlockId(1))]);
        f.locals[0].storage_kind = LirStorageKind::Value;
        f.blocks[0].instrs[0] = compute(
            0,
            LirRvalue::DirectCall {
                callee: factory,
                args: vec![],
                params: vec![],
                result: array.clone(),
            },
        );
        let instrs = &mut f.blocks[1].instrs;
        instrs.resize(stores, instrs[0].clone());
        // This unqueried call lies in the demanded prefix only if a later
        // argument supplies an owner. A call with no arguments adds no prefix.
        instrs.push(compute(
            1,
            LirRvalue::DirectCall {
                callee: prefix_call,
                args: vec![],
                params: vec![],
                result: Type::Void,
            },
        ));
        instrs.push(compute(
            1,
            LirRvalue::DirectCall {
                callee: consumer,
                args: vec![local(0); arity],
                params: vec![array.clone(); arity],
                result: Type::Void,
            },
        ));
        let mut sites: HashSet<_> = (0..stores).map(|i| (1, i)).collect();
        sites.insert((1, stores + 1));
        let roots = collect_dependencies(&f, &HashSet::new(), &sites);
        assert_eq!(DEPENDENCY_QUERIES.get(), (1, 1, 2));
        let mut expected = HashSet::from([factory, consumer]);
        if arity > 0 {
            expected.insert(prefix_call);
        }
        assert_eq!(roots, expected);
    }
}

#[test]
fn returned_callees_are_discovered_through_shared_join_inputs() {
    let left = FunctionId::free_from_source_name("left");
    let right = FunctionId::free_from_source_name("right");
    let unused = FunctionId::free_from_source_name("unqueried");
    for (width, arms) in [(1, 2), (8, 2), (64, 2), (512, 2), (64, 8), (64, 32)] {
        // A diamond followed by a binary branch tree. All its leaves
        // inherit the diamond's join but disagree on an unrelated local.
        let mut preds = vec![vec![], vec![0], vec![0], vec![1, 2]];
        for parent in 3..3 + arms - 1 {
            preds.extend([vec![parent], vec![parent]]);
        }
        let join = preds.len();
        let leaves = join - arms..join;
        preds.push(leaves.clone().collect());
        let mut exits: Vec<_> = preds.iter().map(|_| BlockOrigins::default()).collect();
        exits[0].unknown_entry = true;
        for i in 0..width {
            let id = LirLocalId(i);
            exits[1].set(id, Origin::Returned(left));
            exits[2].set(
                id,
                if i % 2 == 0 {
                    Origin::Returned(right)
                } else {
                    Origin::Unknown
                },
            );
        }
        let other = LirLocalId(width);
        for (i, block) in leaves.enumerate() {
            exits[block].set(
                other,
                if i % 2 == 0 {
                    Origin::Fresh
                } else {
                    Origin::Returned(unused)
                },
            );
        }
        let entries = (0..width).map(|i| (join, LirLocalId(i)));
        let mut callees = HashSet::new();
        collect_returned_callees(&exits, &preds, entries.collect(), &mut callees);
        assert_eq!(callees, HashSet::from([left, right]));
        // All final arms route to the same input per local. The scan
        // retains a key for each distinct entry, with no per-edge payload.
        assert_eq!(
            CALLEE_SCAN.get(),
            (2 * width as usize, arms * width as usize)
        );
    }
}

#[test]
fn equivalent_definitions_on_distinct_diamond_arms_share_the_join() {
    for (diamonds, locals) in [(8, 64), (64, 8), (512, 512)] {
        let size = 3 * diamonds + 1;
        let mut exits: Vec<_> = (0..size).map(|_| BlockOrigins::default()).collect();
        exits[0].unknown_entry = true;
        for id in 0..locals {
            exits[0].set(LirLocalId(id as u32), Origin::Fresh);
        }
        let mut preds = vec![vec![]; size];
        for d in 0..diamonds {
            let b = 3 * d;
            let value = if d % 2 == 0 {
                Origin::Fresh
            } else {
                Origin::Unknown
            };
            for arm in [b + 1, b + 2] {
                preds[arm].push(b);
                exits[arm].set(LirLocalId(locals as u32), value);
            }
            preds[b + 3].extend([b + 1, b + 2]);
        }
        ORIGIN_EDGE_VISITS.set(0);
        let stores = (0..locals).map(|i| (size - 1, LirLocalId(i as u32)));
        let mut callees = HashSet::new();
        collect_returned_callees(&exits, &preds, stores.clone().collect(), &mut callees);
        assert!(callees.is_empty());
        assert_eq!(ORIGIN_EDGE_VISITS.get(), 4 * diamonds);
        assert_eq!(CALLEE_SCAN.get(), (locals, 0));
        let mut index = ReachingDefinitions::new(&exits, &preds);
        for entry in stores {
            index.visit_inputs(entry, |origin| assert!(origin == Origin::Fresh));
        }
    }
}

#[test]
fn repeated_origin_queries_share_the_predecessor_walk() {
    for n in [8, 64, 512, 2048] {
        let id = LirLocalId(0);
        let mut exits: Vec<_> = (0..n).map(|_| BlockOrigins::default()).collect();
        exits[0].set(id, Origin::Fresh);
        let mut preds = vec![Vec::new(); n];
        for (i, row) in preds.iter_mut().enumerate().skip(1) {
            row.push(i - 1);
        }
        ORIGIN_EDGE_VISITS.set(0);
        let entry = (n - 1, id);
        let mut callees = HashSet::new();
        collect_returned_callees(
            &exits,
            &preds,
            std::iter::repeat_n(entry, n).collect(),
            &mut callees,
        );
        assert!(callees.is_empty());
        assert_eq!(CALLEE_SCAN.get(), (1, 0));
        assert_eq!(ORIGIN_EDGE_VISITS.get(), n - 1);
        ReachingDefinitions::new(&exits, &preds)
            .visit_inputs(entry, |origin| assert!(origin == Origin::Fresh));
    }
}

#[test]
fn allocation_proof_is_lost_on_rebinding_or_an_unknown_writer() {
    let mut fresh = BlockOrigins {
        unknown_entry: true,
        ..Default::default()
    };
    let alloc = LirRvalue::ArrayAlloc {
        length: 1,
        element: S,
    };
    let store = LirRvalue::ArrayStore {
        array: local(1),
        index: LirOperand::Int(0),
        value: local(3),
        element: S,
    };
    let call = LirRvalue::DirectCall {
        callee: FunctionId::free_from_source_name("unknown"),
        args: vec![],
        params: vec![],
        result: S,
    };
    let mut writes = |dest, value: &LirRvalue| {
        may_write_aliased_storage(
            LirLocalId(dest),
            value,
            &mut fresh,
            &mut FieldOrigins::default(),
            |_, _, _, _| CallTransfer::Opaque,
        ) != Origin::Fresh
    };
    assert!(!writes(0, &alloc));
    assert!(!writes(1, &LirRvalue::Use(local(0))));
    assert!(!writes(2, &store));
    assert!(!writes(1, &LirRvalue::Use(local(3))));
    assert!(writes(2, &store));
    assert!(!writes(1, &alloc));
    assert!(writes(0, &call));
    assert!(writes(2, &store));
}

#[test]
fn mutating_intrinsics_and_unresolved_calls_block_borrowed_reads() {
    let program = lowered(
        r#"
            import std::collections::Map;
            fn store(m: Map<String, i64>) -> String {
                m.insert("k", 1);
                return "!";
            }
            fn via_map(x: &String, m: Map<String, i64>) -> String {
                return x + ":" + store(m);
            }
            fn via_callback(x: &String, f: fn() -> String) -> String {
                return x + ":" + f();
            }
        "#,
    );
    assert!(values(function(&program, "store")).iter().any(|v| matches!(
        v,
        LirRvalue::IntrinsicCall {
            intrinsic: Intrinsic::MapInsert,
            ..
        }
    )));
    for name in ["via_map", "via_callback"] {
        assert_saved_string(function(&program, name));
    }
}

#[test]
fn fresh_intrinsic_results_and_written_arguments_preserve_concat_fusion() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn grow(other:Array<String>, items:Array<String>, n:String) {
                items.push(n);
                items[0] = n;
            }
            fn forward(a:Array<String>, b:Array<String>, n:String) { grow(b,a,n); }
            fn split(n:String) -> String {
                let items=n.split(",");
                if n.len() == 8 {return n;}
                items.push(n);
                return n;
            }
            fn private(other:Array<String>, n:String) -> String {
                let items=[n,n]; forward(items,other,n); return n;
            }
            fn shared(other:Array<String>, n:String) -> String {
                let items=[n,n]; forward(other,items,n); return n;
            }
            fn mixed(other:Array<String>,n:String) -> String {
                let mut items=n.split(",");
                if n.len() == 8 { items=other; }
                forward(items,other,n); return n;
            }
            fn via_split(x:&String,n:String)->String { return x+":"+split(n); }
            fn via_private(x:&String,n:String,a:Array<String>)->String { return x+":"+private(a,n); }
            fn via_shared(x:&String,n:String,a:Array<String>)->String { return x+":"+shared(a,n); }
            fn via_mixed(x:&String,n:String,a:Array<String>)->String { return x+":"+mixed(a,n); }
        "#,
    );
    for name in ["via_split", "via_private"] {
        assert_eq!(
            concats(function(&program, name)),
            vec![vec![S, S, S]],
            "{name}"
        );
    }
    for name in ["via_shared", "via_mixed"] {
        assert_saved_string(function(&program, name));
    }
}

#[test]
fn a_mutating_intrinsic_can_return_a_new_container() {
    let program = lowered(
        r#"
            import std::collections::Map;
            fn take(m:Map<String,String>)->Option<String> { return m.remove("k"); }
        "#,
    );
    let take = function(&program, "take");
    let summaries = summarize_callees(&program.functions, HashSet::from([take.name]));
    let out = summaries.functions[&take.name];
    assert_eq!(
        summaries.graph.facts[out.writes],
        HashSet::from([Fact::Value(0)])
    );
    assert!(summaries.graph.facts[out.result].is_empty());
}

#[test]
fn fresh_arguments_survive_calls_before_the_deferred_read() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn label(n:String)->String {if n.len()==0 {return "ok";} return n;}
            fn grow(a:Array<String>,n:String)->String {a.push(n); return n;}
            fn local(x:&String,n:String)->String {
                let a=n.split(","); let s=label(n); return x+":"+grow(a,s);
            }
            fn branched(x:&String,n:String)->String {
                let a=n.split(","); let s=label(n); if s.len()==8 {return n;}
                return x+":"+grow(a,s);
            }
        "#,
    );
    for name in ["local", "branched"] {
        assert_eq!(
            concats(function(&program, name)),
            vec![vec![S, S, S]],
            "{name}"
        );
    }
}

#[test]
fn field_reads_use_the_last_stored_owner_on_the_same_object() {
    let program = lowered(
        r#"
            import std::collections::Array;
            class Holder { pub items: Array<String>; }
            class Pair { pub left: Array<String>; pub right: Array<String>; }
            fn grow(items:Array<String>,n:String)->String { items.push(n); return n; }
            fn replace(h:Holder,a:Array<String>) { h.items=a; }
            fn private(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); let b=new Holder(other);
                return grow(a.items,n);
            }
            fn shared(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); let b=new Holder(other);
                return grow(b.items,n);
            }
            fn overwritten(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); let alias=a; alias.items=other;
                return grow(a.items,n);
            }
            fn replaced(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); replace(a,other);
                return grow(a.items,n);
            }
            fn restored(n:String, other:Array<String>)->String {
                let a=new Holder(other); let alias=a; alias.items=[n,n];
                return grow(a.items,n);
            }
            fn snapshot(n:String, other:Array<String>)->String {
                let mut items=[n,n]; let a=new Holder(items); items=other;
                return grow(a.items,n);
            }
            fn left(n:String, other:Array<String>)->String {
                let h=new Pair([n,n],other); return grow(h.left,n);
            }
            fn right(n:String, other:Array<String>)->String {
                let h=new Pair([n,n],other); return grow(h.right,n);
            }
            fn via_left(x:&String,n:String,a:Array<String>)->String { return x+":"+left(n,a); }
            fn via_right(x:&String,n:String,a:Array<String>)->String { return x+":"+right(n,a); }
            fn via_private(x:&String,n:String,a:Array<String>)->String { return x+":"+private(n,a); }
            fn via_shared(x:&String,n:String,a:Array<String>)->String { return x+":"+shared(n,a); }
            fn via_overwrite(x:&String,n:String,a:Array<String>)->String { return x+":"+overwritten(n,a); }
            fn via_replace(x:&String,n:String,a:Array<String>)->String { return x+":"+replaced(n,a); }
            fn via_restore(x:&String,n:String,a:Array<String>)->String { return x+":"+restored(n,a); }
            fn via_snapshot(x:&String,n:String,a:Array<String>)->String { return x+":"+snapshot(n,a); }
        "#,
    );
    for name in ["via_private", "via_restore", "via_snapshot", "via_left"] {
        assert_eq!(
            concats(function(&program, name)),
            vec![vec![S, S, S]],
            "{name}"
        );
    }
    for name in ["via_shared", "via_overwrite", "via_replace", "via_right"] {
        assert_saved_string(function(&program, name));
    }
}

#[test]
fn a_field_read_before_a_private_allocation_adds_no_callee_dependency() {
    SUMMARY_BODIES.set(0);
    let program = lowered(
        r#"
            import std::collections::Array;
            class Holder {pub items:Array<String>;}
            fn label(a:Array<String>,n:String)->String {if a.len()==0{return "empty";} return n;}
            fn grow(a:Array<String>,n:String)->String {a.push(n);return n;}
            fn render(x:&String,n:String,h:Holder)->String {
                let value=label(h.items,n);
                let a=[n,n]; return x+":"+grow(a,value);
            }
        "#,
    );
    let render = function(&program, "render");
    let label = function(&program, "label").name;
    assert!(
        values(render)
            .iter()
            .any(|v| matches!(v, LirRvalue::DirectCall{callee,..} if *callee == label))
    );
    assert_eq!(concats(render), vec![vec![S, S, S]]);
    assert_eq!(SUMMARY_BODIES.get(), 1);
}

#[test]
fn distinct_objects_and_repeated_field_reads_use_one_lookup_per_access() {
    for (objects, reads) in [(1, 1), (8, 64), (64, 8), (128, 256)] {
        let mut source = String::from(
            r#"
                import std::collections::Array;
                class Holder {pub items:Array<String>;}
                fn helper(n:String)->String { let items=[n,n];
            "#,
        );
        for i in 0..objects {
            source.push_str(&format!("let h{i}=new Holder(items);\n"));
        }
        for i in 0..reads {
            source.push_str(&format!("h{}.items.push(n);\n", i % objects));
        }
        source.push_str(
            r#"return n;}
                fn render(x:&String,n:String)->String {return x+":"+helper(n);}"#,
        );
        FIELD_STORES.set(0);
        FIELD_READS.set(0);
        PEAK_FIELDS.set(0);
        let program = lowered(&source);
        assert_eq!(concats(function(&program, "render")), vec![vec![S, S, S]]);
        assert_eq!(
            (FIELD_STORES.get(), FIELD_READS.get(), PEAK_FIELDS.get()),
            (objects, reads, objects)
        );
    }
}

#[test]
fn returned_parameters_bind_to_each_calls_actual_owners() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn choose(a:Array<String>, b:Array<String>, n:String)->Array<String> {
                if n.len()==8 {return a;} return b;
            }
            fn identity(a:Array<String>, n:String)->Array<String> {
                if n.len()==8 {return a;} return a;
            }
            fn swap(a:Array<String>, b:Array<String>, n:String)->Array<String> {
                return choose(b,a,n);
            }
            fn grow(items:Array<String>, n:String)->String {items.push(n); return n;}
            fn private(x:&String,n:String)->String {
                let a=identity([n,n],n); return x+":"+grow(a,n);
            }
            fn shared(x:&String,input:Array<String>,n:String)->String {
                let a=identity(input,n); return x+":"+grow(a,n);
            }
            fn mixed(x:&String,input:Array<String>,n:String)->String {
                let a=swap([n,n],input,n); return x+":"+grow(a,n);
            }
            fn both_private(x:&String,n:String)->String {
                let a=swap(n.split(","),[n,n],n); return x+":"+grow(a,n);
            }
        "#,
    );
    for name in ["private", "both_private"] {
        assert_eq!(
            concats(function(&program, name)),
            vec![vec![S, S, S]],
            "{name}"
        );
    }
    for name in ["shared", "mixed"] {
        assert_saved_string(function(&program, name));
    }
}

#[test]
fn repeated_reference_arguments_share_a_post_call_origin() {
    for arity in [1, 8, 64, 256] {
        let params = (0..arity)
            .map(|i| format!("a{i}:&Array<String>"))
            .collect::<Vec<_>>()
            .join(",");
        let args = vec!["&items"; arity].join(",");
        let program = lowered(&format!(
            r#"
                import std::collections::Array;
                fn inspect({params})->i64 {{
                    if a0.len()==0 {{return 0;}} return 1;
                }}
                fn caller()->Array<String> {{
                    let items=["private"]; inspect({args}); return items;
                }}
            "#
        ));
        let caller = function(&program, "caller");
        let direct = crate::ir::lowered::synchronous_function_indices(&program.functions);
        let mut analysis = CallAnalysis::default();
        let out = analysis.outputs(caller.name, &direct).unwrap();
        analysis.body(
            caller,
            &HashSet::new(),
            Demand::Function(out),
            None,
            &direct,
        );
        let callee = function(&program, "inspect");
        let inspect = analysis.functions[&callee.name];
        let mut destinations = HashSet::new();
        for i in 0..arity {
            // Every actual supplies the original fresh value, including the last.
            assert!(
                !analysis
                    .graph
                    .bindings
                    .contains_key(&(inspect.writes, Fact::Value(i)))
            );
            let bindings = &analysis.graph.bindings[&(inspect.writes, Fact::Slot(i))];
            assert_eq!(bindings.len(), 1);
            for &(source, dest) in bindings {
                assert!(source == Source::Fact(Fact::External));
                destinations.insert(dest);
            }
        }
        assert_eq!(destinations.len(), 1);
        let after = *destinations.iter().next().unwrap();
        assert_eq!(analysis.graph.copies[&after], HashSet::from([out.result]));
        // Two outputs each for caller and callee, plus the single post-call value.
        assert_eq!(analysis.graph.facts.len(), 5);
        analysis.body(
            callee,
            &reference_parameters(callee),
            Demand::Function(inspect),
            None,
            &direct,
        );
        analysis.graph.solve();
        assert!(analysis.graph.facts[out.result].is_empty());
    }
}

#[test]
fn mutable_reference_rebinds_only_the_written_array_slot() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn rebind(keep:&Array<String>, target:&mut Array<String>, input:Array<String>) {
                target=input;
            }
            fn kept(input:Array<String>)->Array<String> {
                let keep=["private"]; let mut target=["initial"];
                rebind(&keep,&target,input); return keep;
            }
            fn changed(prior:Array<String>, input:Array<String>)->Array<String> {
                let keep=["private"]; let mut target=prior;
                rebind(&keep,&target,input); return target;
            }
        "#,
    );
    let kept = function(&program, "kept").name;
    let changed = function(&program, "changed").name;
    let analysis = summarize_callees(&program.functions, HashSet::from([kept, changed]));
    assert!(analysis.graph.facts[analysis.functions[&kept].result].is_empty());
    assert_eq!(
        analysis.graph.facts[analysis.functions[&changed].result],
        HashSet::from([Fact::Value(0), Fact::External])
    );
}

#[test]
fn reference_slot_writes_bind_to_the_actual_place() {
    let program = lowered(
        r#"
            import std::collections::Array;
            fn change(value:&mut String)->String {value="new"; return "!";}
            fn forward(value:&mut String)->String {return change(&value);}
            fn helper(n:String)->String {let mut tmp=n; forward(&tmp); return tmp;}
            fn private(x:&String,n:String)->String {return x+":"+helper(n);}
            fn distinct(x:&String,n:String)->String {
                let mut tmp=n; return x+":"+forward(&tmp);
            }
            fn same(n:String)->String {
                let mut tmp=n; return tmp+":"+forward(&tmp);
            }
            fn element(x:&String,items:Array<String>)->String {
                return x+":"+forward(&items[0]);
            }
            fn rebind(dst:&mut Array<String>,input:Array<String>) {dst=input;}
            fn replace(input:Array<String>)->String {
                let mut tmp=["local"]; rebind(&tmp,input); tmp[0]="new"; return "!";
            }
            fn rebound(x:&String,input:Array<String>)->String {
                return x+":"+replace(input);
            }
        "#,
    );
    for name in ["private", "distinct"] {
        assert_eq!(
            concats(function(&program, name)),
            vec![vec![S, S, S]],
            "{name}"
        );
    }
    for name in ["same", "element", "rebound"] {
        assert_saved_string(function(&program, name));
    }
}

#[test]
fn forwarded_return_origins_propagate_once_per_wrapper() {
    for n in [1, 8, 64, 256] {
        let mut source = String::from(
            "import std::collections::Array; fn wrap_0(a:Array<String>)->Array<String>{return a;}\n",
        );
        for i in 1..=n {
            source.push_str(&format!(
                "fn wrap_{i}(a:Array<String>)->Array<String>{{return wrap_{}(a);}}\n",
                i - 1
            ));
        }
        let program = lowered(&source);
        let root = function(&program, &format!("wrap_{n}")).name;
        PROPAGATED_CALL_EDGES.set(0);
        let analysis = summarize_callees(&program.functions, HashSet::from([root]));
        assert_eq!(analysis.functions.len(), n + 1);
        assert_eq!(SUMMARY_EDGES.get(), 4 * n);
        assert_eq!(PROPAGATED_CALL_EDGES.get(), n);
        for out in analysis.functions.values() {
            assert!(analysis.graph.facts[out.writes].is_empty());
            assert_eq!(
                analysis.graph.facts[out.result],
                HashSet::from([Fact::Value(0)])
            );
        }
    }
}

#[test]
fn write_summary_visits_only_callees_of_crossed_reads() {
    for n in [0, 8, 64, 256] {
        let mut source = String::from(
            r#"
                import std::collections::Array;
                fn grow(a:Array<String>,n:String) {a.push(n);}
                fn helper(n:String)->String {let a=n.split(","); grow(a,n); return n;}
                fn unrelated(x:&String,n:String)->String {return helper(n);}
            "#,
        );
        for i in 0..n {
            source.push_str(&format!(
                "fn unused_{i}(x:&String,n:String)->String {{return unrelated(&x,n);}}\n"
            ));
        }
        for active in [false, true] {
            crate::ir::lowered::DIRECT_INDEX_WORK.set((0, 0));
            SUMMARY_INDEX_PROBES.set(0);
            SUMMARY_BODIES.set(0);
            SUMMARY_EDGES.set(0);
            let program = lowered(&source);
            assert_eq!(
                crate::ir::lowered::DIRECT_INDEX_WORK.get(),
                (1, n + 3 + usize::from(active))
            );
            let retained = if active { 2 } else { 0 };
            assert_eq!(
                (SUMMARY_INDEX_PROBES.get(), SUMMARY_LOOKUP_ENTRIES.get()),
                (retained, retained)
            );
            if active {
                assert_eq!(concats(function(&program, "render")), vec![vec![S, S, S]]);
                assert_eq!((SUMMARY_BODIES.get(), SUMMARY_EDGES.get()), (2, 1), "N={n}");
            } else {
                assert_eq!((SUMMARY_BODIES.get(), SUMMARY_EDGES.get()), (0, 0), "N={n}");
                source.push_str(
                    r#"
                       fn render(x:&String,n:String)->String {
                           let unused=unrelated(&x,n);
                           return x+":"+helper(n);
                       }"#,
                );
            }
        }
    }
}

#[test]
fn repeated_calls_store_and_propagate_one_write_edge() {
    let program = lowered(
        r#"
            fn change(x: &mut String) -> String { x = "new"; return "!"; }
            fn caller(x: &mut String) -> String { return change(&x); }
        "#,
    );
    let caller = function(&program, "caller");
    let writer = function(&program, "change");
    let is_writer_call = |inst: &SourceInst| {
        matches!(
            inst,
            SourceInst::Compute { value: LirRvalue::DirectCall { callee, .. }, .. }
                if *callee == writer.name
        )
    };
    for n in [1, 8, 64, 512, 2048] {
        let mut caller = caller.clone();
        let instrs = &mut caller.blocks[0].instrs;
        let index = instrs.iter().position(is_writer_call).unwrap();
        let call = instrs[index].clone();
        instrs.splice(index..=index, std::iter::repeat_n(call, n));
        let targets = HashSet::from([caller.name, writer.name]);
        PROPAGATED_CALL_EDGES.set(0);
        let read_only = summarize_callees(&[caller, writer.clone()], targets);
        for out in read_only.functions.values() {
            assert_eq!(
                read_only.graph.facts[out.writes],
                HashSet::from([Fact::Slot(0)])
            );
        }
        assert_eq!(STORED_CALL_EDGES.get(), 1);
        assert_eq!(SUMMARY_EDGES.get(), 2);
        assert_eq!(PROPAGATED_CALL_EDGES.get(), 1);
    }
}

#[test]
fn suspend_access_counts_the_read_after_a_local_definition() {
    let mut f = hand_built(0, 1, vec![compute(0, LirRvalue::Use(LirOperand::Int(1)))]);
    f.locals[0].ty = Type::I64;
    f.blocks[0].terminator = SourceTerminator::Suspend {
        operation: crate::ir::lowered::SuspendOp::Sleep {
            millis: LirLocalId(0),
        },
        resume: BlockId(0),
    };
    let accesses = Accesses::count(&f);
    assert_eq!(accesses.reads, vec![1]);
    assert_eq!(accesses.writes, vec![1]);
}

#[test]
fn a_write_to_a_deferred_operand_blocks_absorption() {
    let f = fused(
        vec![
            add(3, local(0), local(1)),
            compute(0, LirRvalue::Use(local(2))),
            add(4, local(3), local(2)),
        ],
        3,
        2,
    );
    assert!(concats(&f).is_empty());
    assert_eq!(f.blocks[0].instrs.len(), 3);
    // A write to an unrelated local does not.
    let f = fused(
        vec![
            add(3, local(0), local(1)),
            compute(2, LirRvalue::Use(local(0))),
            add(4, local(3), local(2)),
        ],
        3,
        2,
    );
    assert_eq!(concats(&f), vec![vec![S, S, S]]);
    assert_eq!(f.blocks[0].instrs.len(), 2);
}

#[test]
fn a_write_invalidates_the_whole_absorbed_tree() {
    // `l0` is a leaf of `t3`, which `t4` absorbed; overwriting `l0` before
    // `t5` must stop `t5` from deferring `t3`'s read of it.
    let f = fused(
        vec![
            add(3, local(0), local(1)),
            add(4, local(3), local(2)),
            compute(0, LirRvalue::Use(local(2))),
            add(5, local(4), local(1)),
        ],
        3,
        3,
    );
    assert_eq!(concats(&f), vec![vec![S, S, S]]);
    assert_eq!(string_adds(&f), 1);
}

#[test]
fn non_compute_instructions_are_barriers() {
    let f = fused(
        vec![
            add(3, local(0), local(1)),
            SourceInst::ClearScopeRoots { locals: Vec::new() },
            add(4, local(3), local(2)),
        ],
        3,
        2,
    );
    assert!(concats(&f).is_empty());
}

#[test]
fn heap_places_are_never_deferred() {
    let place = LirOperand::Reference {
        place: LirPlace::Local(LirLocalId(1)),
        span: Span::dummy(),
        display: "l1".into(),
    };
    let f = fused(
        vec![add(3, local(0), place), add(4, local(3), local(2))],
        3,
        2,
    );
    assert!(concats(&f).is_empty());
}

#[test]
fn a_self_updating_temporary_chain_fuses() {
    let f = fused(
        vec![
            compute(3, LirRvalue::Use(local(0))),
            add(3, local(3), local(1)),
            add(3, local(3), local(2)),
            compute(4, LirRvalue::Use(local(3))),
        ],
        3,
        2,
    );
    assert_eq!(concats(&f), vec![vec![S, S, S]]);
    assert_eq!(f.blocks[0].instrs.len(), 2);
}

#[test]
fn a_long_chain_is_flattened_without_recursion() {
    const PARTS: u32 = 100_000;
    let mut instrs = vec![add(2, local(0), local(1))];
    for temp in 3..PARTS + 1 {
        instrs.push(add(temp, local(temp - 1), local(0)));
    }
    let f = fused(instrs, 2, PARTS - 1);
    assert_eq!(f.blocks[0].instrs.len(), 1);
    let parts = concats(&f);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].len(), PARTS as usize);
}
