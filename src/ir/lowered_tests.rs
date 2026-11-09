#[test]
fn async_array_reads_have_only_backedge_safepoints() {
    for ty in ["Array<i64>", "FrozenArray<i64>"] {
        for loops in [1, 4, 16, 64] {
            let body = "i = 0; while i < xs.len() { total = total + xs[i]; i = i + 1; }";
            let source = format!(
                "async fn f(xs: {ty}) -> i64 {{ let mut total = 0; let mut i = 0; {} return total; }}",
                body.repeat(loops)
            );
            let program = lir_checked(&source);
            let f = func(&program, "f");
            let polls = f
                .blocks
                .iter()
                .filter(|b| {
                    matches!(
                        b.terminator,
                        SourceTerminator::Suspend {
                            operation: SuspendOp::Preempt,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(polls, loops, "one poll per loop, type={ty} loops={loops}");
            println!(
                "type={ty} loops={loops} preempt_edges={polls} blocks={}",
                f.blocks.len()
            );
        }
    }
}

#[test]
fn async_array_operand_calls_keep_their_safepoints() {
    for expr in ["xs[index(i)]", "get(xs)[i]", "get(xs).len()"] {
        let program = lir_checked(&format!(
            "fn index(i: i64) -> i64 {{ return i; }} fn get(xs: Array<i64>) -> Array<i64> {{ return xs; }} async fn f(xs: Array<i64>) -> i64 {{ let mut i = 0; let mut total = 0; while i < xs.len() {{ total = total + {expr}; i = i + 1; }} return total; }}"
        ));
        let f = func(&program, "f");
        let polls = f
            .blocks
            .iter()
            .filter(|b| {
                matches!(
                    b.terminator,
                    SourceTerminator::Suspend {
                        operation: SuspendOp::Preempt,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(polls, 2, "operand call must keep its boundary: {expr}");
    }
}

#[test]
fn sync_bodies_skip_final_async_analysis() {
    for n in [1, 8, 64] {
        let source = (0..n).map(|i| format!(
                "fn f{i}(x: i64) {{ defer {{ print(x); }} let callback = |y: i64| -> i64 {{ defer {{ print(y); }} print(x); return y; }}; callback(x); }}"
            )).collect::<String>();
        FINAL_ASYNC_ANALYSES.with(|count| count.set(0));
        let program = lir(&source);
        assert_eq!(program.functions.len(), n);
        assert_eq!(program.lambdas.len(), n);
        for function in program
            .functions
            .iter()
            .chain(program.lambdas.iter().map(|lambda| &lambda.function))
        {
            assert!(!function.is_async);
            assert_eq!(function.async_frame, LirAsyncFrameLayout::default());
        }
        assert_eq!(FINAL_ASYNC_ANALYSES.with(|count| count.get()), 0);
        println!("sync functions={n} lambdas={n} final_async_analyses=0 frame_slots=0");
    }
}

#[test]
fn async_layout_matches_final_transformed_graph() {
    FINAL_ASYNC_ANALYSES.with(|count| count.set(0));
    let program = lir(
        "async fn f(s: String) { defer { print(s); } let first = 1; await yield(); print(first); let second = 2; await yield(); print(second); print(s); }",
    );
    let function = func(&program, "f");
    assert_eq!(FINAL_ASYNC_ANALYSES.with(|count| count.get()), 1);
    assert!(function.is_async);
    assert!(!function.async_frame.slots.is_empty());
    assert_eq!(
        function.async_frame,
        async_liveness::analyze(&function.blocks, &function.locals)
    );
    let captured = function
        .locals
        .iter()
        .find(|local| local.name == "s")
        .unwrap();
    assert!(function.async_frame.slot(captured.id).is_some());
}

#[test]
fn async_bodies_solve_liveness_once_per_lowered_body() {
    // Root clearing and frame planning share one solve per body; earlier
    // lowering and optimization stages no longer plan discarded frames.
    for n in [1, 4, 16] {
        let source = (0..n)
                .map(|i| format!(
                    "async fn f{i}(s: String) {{ defer {{ print(s); }} let a = 1; await yield(); print(a); let callback = |y: i64| -> i64 {{ return y; }}; print(callback(a)); }}"
                ))
                .collect::<String>();
        liveness::SOLVES.with(|count| count.set(0));
        FINAL_ASYNC_ANALYSES.with(|count| count.set(0));
        let program = lir(&source);
        let mut bodies = 0;
        let mut pending: Vec<_> = program
            .functions
            .iter()
            .chain(program.lambdas.iter().map(|lambda| &lambda.function))
            .collect();
        while let Some(function) = pending.pop() {
            bodies += 1;
            for inst in function.blocks.iter().flat_map(|block| &block.instrs) {
                if let SourceInst::Defer { body, .. } = inst {
                    pending.push(&body.function);
                }
            }
        }
        assert_eq!(bodies, 3 * n);
        assert_eq!(FINAL_ASYNC_ANALYSES.with(|count| count.get()), n);
        assert_eq!(liveness::SOLVES.with(|count| count.get()), bodies);
        for i in 0..n {
            let function = func(&program, &format!("f{i}"));
            assert_eq!(
                function.async_frame,
                async_liveness::analyze(&function.blocks, &function.locals)
            );
        }
        println!("async functions={n} lowered bodies={bodies} liveness solves={bodies}");
    }
}

#[test]
fn nested_async_loop_liveness_work_is_linear_in_result_size() {
    // Each level suspends inside its own loop and a string defined before
    // the nest is read innermost, so liveness crosses every backedge.
    // The former reverse-order rescans needed a round per nesting level.
    let mut previous = None;
    for depth in [25, 50, 100, 200] {
        let mut source = String::from("async fn work() { let keep = \"kept\";");
        for level in 0..depth {
            source.push_str(&format!(
                    "let mut k{level} = 0; while k{level} < 1 {{ await yield(); k{level} = k{level} + 1;"
                ));
        }
        source.push_str("print(keep);");
        source.push_str(&"}".repeat(depth));
        source.push('}');
        let program = lir(&source);
        let work = func(&program, "work");
        let live = liveness::Liveness::compute(&work.blocks, &work.locals);
        let edges: usize = work
            .blocks
            .iter()
            .map(|block| {
                liveness::normal_successors(&block.terminator).count() + block.recovery.len()
            })
            .sum();
        let entries: usize = (0..work.blocks.len())
            .map(|block| live.live_in.iter(block).count())
            .sum();
        println!(
            "nested async loops depth={depth} blocks={} edges={edges} locals={} live_in_entries={} edge_visits={} set_bytes={}",
            work.blocks.len(),
            work.locals.len(),
            live.stats.entries,
            live.stats.edge_visits,
            live.set_bytes(),
        );
        assert_eq!(live.stats.entries, entries);
        // The result itself grows with depth * blocks (each loop counter
        // stays live through every deeper loop); the solver performs one
        // edge visit per carried membership, with no per-level rescans.
        let per_entry = live.stats.edge_visits as f64 / entries as f64;
        assert!(per_entry <= 2.0, "{per_entry}");
        if let Some(previous) = previous {
            assert!(per_entry <= previous * 1.1, "{per_entry} after {previous}");
        }
        previous = Some(per_entry);
    }
}

#[test]
fn defer_identity_survives_twenty_shared_span_shapes() {
    for count in 1..=20 {
        let source = format!("fn f() {{ {} }}", "defer print(1);".repeat(count));
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (mut ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let crate::parser::ast::Item::Function(f) = &mut ast.items[0] else {
            unreachable!()
        };
        for stmt in &mut f.body.stmts {
            if let crate::parser::ast::Stmt::Defer(defer) = stmt {
                defer.span = f.span;
            }
        }
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty(), "{errors:?}");
        let lir = lower_source_program(&hir);
        let ids: std::collections::HashSet<_> = lir.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                if let SourceInst::Defer { id, .. } = inst {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ids.len(), count, "same-span defer sites must stay distinct");
    }
}

#[test]
fn deep_expression_cfg_lowering_uses_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let span = Span::dummy();
            let int = |n| HirExpr {
                kind: HirExprKind::Int(n),
                ty: Type::I64,
                span,
            };
            let mut expr = HirExpr {
                kind: HirExprKind::Ternary {
                    condition: Box::new(HirExpr {
                        kind: HirExprKind::Bool(true),
                        ty: Type::Bool,
                        span,
                    }),
                    then_expr: Box::new(int(1)),
                    else_expr: Box::new(int(2)),
                },
                ty: Type::I64,
                span,
            };
            for _ in 0..50_000 {
                expr = HirExpr {
                    kind: HirExprKind::Unary {
                        op: crate::parser::ast::UnaryOp::Neg,
                        operand: Box::new(expr),
                    },
                    ty: Type::I64,
                    span,
                };
            }
            let mut builder = Builder::new(&[], false);
            let lowered = builder
                .lower_nested_cfg(&expr)
                .expect("nested control flow");
            assert!(!expression_needs_cfg(&lowered));
            assert!(
                builder
                    .blocks
                    .iter()
                    .any(|(_, term)| matches!(term, Some(SourceTerminator::Branch { .. })))
            );
            assert!(
                builder
                    .blocks
                    .iter()
                    .map(|(insts, _)| insts.len())
                    .sum::<usize>()
                    >= 50_000
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn deep_hir_predicates_use_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            for has_call in [false, true] {
                let span = Span::new(0, 0, 1, 1);
                let mut expr = HirExpr {
                    kind: if has_call {
                        HirExprKind::Call {
                            callee: "recover".into(),
                            args: Vec::new(),
                        }
                    } else {
                        HirExprKind::Int(1)
                    },
                    ty: Type::I64,
                    span,
                };
                for _ in 0..50_000 {
                    expr = HirExpr {
                        kind: HirExprKind::TryPropagate {
                            inner: Box::new(expr),
                        },
                        ty: Type::I64,
                        span,
                    };
                }
                assert_eq!(expr_has_recover(&expr), has_call);
                assert_eq!(expression_executes_call(&expr), has_call);
                let mut lambdas = Vec::new();
                collect_lambdas_in_expr(&expr, &mut lambdas, &Default::default());
                assert!(lambdas.is_empty());
                drop(expr);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn deep_hir_suspension_collection_uses_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let span = Span::new(0, 0, 1, 1);
            let mut expr = HirExpr {
                kind: HirExprKind::Int(1),
                ty: Type::I64,
                span,
            };
            expr = HirExpr {
                kind: HirExprKind::Await {
                    inner: Box::new(expr),
                },
                ty: Type::I64,
                span,
            };
            for _ in 0..50_000 {
                expr = HirExpr {
                    kind: HirExprKind::TryPropagate {
                        inner: Box::new(expr),
                    },
                    ty: Type::I64,
                    span,
                };
            }
            let mut found = Vec::new();
            collect_suspensions(&expr, &mut found);
            assert_eq!(found.len(), 1);
            assert!(suspends_anywhere(&expr));
            assert!(contains_suspend_span(&expr, span));
            drop(found);
            drop(expr);
        })
        .unwrap()
        .join()
        .unwrap();
}

use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;

/// Parse + HIR-lower + LIR-lower; assert no HIR diagnostics.
fn lir(src: &str) -> SourceProgram {
    let tokens = Lexer::new(src).tokenize().expect("lexing failed");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "parse errors: {errs:?}");
    let (hir, diags) = super::super::lower::lower_program(&program);
    assert!(diags.is_empty(), "HIR diagnostics: {diags:?}");
    lower_source_program(&hir)
}

fn lir_checked(src: &str) -> SourceProgram {
    let source = format!("import std::collections::Array; {src}");
    let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).unwrap();
    checker.check_program(&program);
    assert!(
        checker
            .errors
            .iter()
            .all(|diagnostic| diagnostic.severity != crate::diagnostics::Severity::Error),
        "{:?}",
        checker.errors
    );
    let facts = crate::ir::lower::SemanticFacts::from_checker(&checker);
    let (hir, errors) = crate::ir::lower::lower_program_with(&program, &facts);
    assert!(errors.is_empty(), "{errors:?}");
    lower_source_program(&hir)
}

fn func<'a>(p: &'a SourceProgram, name: &str) -> &'a SourceFunction {
    p.functions
        .iter()
        .find(|f| f.name.to_string() == name)
        .unwrap_or_else(|| panic!("no function {name}"))
}

#[test]
fn frozen_iteration_lowering_scales_with_loop_count() {
    // Compare identical mutable/frozen bodies: no unrolling, copying, or
    // extra cleanup graph is introduced for an immutable handle.
    for count in [1, 8, 16, 32] {
        let body = "for x in values { if x == 1 { continue; } if x == 2 { break; } println(x); }"
            .repeat(count);
        let mut shapes = Vec::new();
        for ty in ["Array<i64>", "FrozenArray<i64>"] {
            let program = lir_checked(&format!("fn visit(values: {ty}) {{ {body} }}"));
            let f = func(&program, "visit");
            let shape = (
                f.blocks.len(),
                f.blocks.iter().map(|b| b.instrs.len()).sum::<usize>(),
                f.locals.len(),
            );
            eprintln!(
                "frozen iteration loops={count} type={ty} blocks={} instructions={} locals={}",
                shape.0, shape.1, shape.2
            );
            shapes.push(shape);
        }
        assert_eq!(shapes[0], shapes[1]);
        // Fixed overhead plus constant work per source loop, including
        // adverse multiple exits. Counts are deterministic, not timings.
        assert_eq!(shapes[1], (8 * count + 1, 18 * count, 14 * count + 1));
    }
}

fn scope_clear_entries(function: &SourceFunction) -> usize {
    function
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .map(|inst| {
            if let SourceInst::ClearScopeRoots { locals } = inst {
                locals.len()
            } else {
                0
            }
        })
        .sum()
}

#[test]
fn nested_scope_clear_entries_are_linear() {
    // Source scalars, GC owners, async frames, and sequential fan-out.
    for shape in ["nested", "owners", "async", "siblings"] {
        for n in [25, 50, 100, 200] {
            let mut source = String::from("class Payload { pub value: i64; }\n");
            if shape == "async" {
                source.push_str("async ");
            }
            source.push_str("fn f() { let keep = 1;\n");
            for i in 0..n {
                source.push_str(&format!(
                    "let mut k{i} = 0; while k{i} < 1 {{ k{i} = k{i} + 1;\n"
                ));
                if shape == "owners" {
                    source.push_str(&format!(
                        "let p{i} = new Payload({i}); print(p{i}.value);\n"
                    ));
                }
                if shape == "async" {
                    source.push_str("await yield();\n");
                }
                if shape == "siblings" {
                    source.push_str("}\n");
                }
            }
            source.push_str("print(keep);\n");
            if shape != "siblings" {
                source.push_str(&"}\n".repeat(n));
            }
            source.push_str("}\n");
            let program = lir(&source);
            let count = scope_clear_entries(func(&program, "f"));
            // Owners also receive one existing last-use clear per object.
            let expected = if shape == "owners" { 3 * n + 1 } else { n + 1 };
            assert_eq!(count, expected, "{shape} N={n}");
            println!("scope-clear {shape} N={n} entries={count}");
        }
    }
}

#[test]
fn recovery_still_clears_bypassed_child_scopes() {
    let program = lir(
        "fn f() { defer match recover() { Some(info) => {}, None => {} } if true { let child = 42; print(child); } }",
    );
    let f = func(&program, "f");
    let child = f
        .locals
        .iter()
        .find(|local| local.name == "child")
        .unwrap()
        .id;
    let clears = f.blocks.iter().flat_map(|block| &block.instrs).filter(|inst| {
            matches!(inst, SourceInst::ClearScopeRoots { locals } if locals.contains(&child))
        }).count();
    assert_eq!(clears, 2, "ordinary child close plus recovery continuation");
}

#[test]
fn defer_cleanup_region_owns_cfg_and_captures_only_outer_reads_or_writes() {
    let program = lir(
        "fn f(flag: bool) { let mut used = 1; let unused = 2; defer { let own = 3; while flag { used = own; } } }",
    );
    let outer = func(&program, "f");
    let cleanup = outer
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .find_map(|inst| {
            if let SourceInst::Defer { body, .. } = inst {
                Some(body)
            } else {
                None
            }
        })
        .unwrap();
    let captures: Vec<_> = cleanup
        .captures
        .iter()
        .map(|id| outer.locals[id.0 as usize].name.as_str())
        .collect();
    assert_eq!(captures, ["flag", "used"]);
    assert!(
        cleanup
            .function
            .blocks
            .iter()
            .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. }))
    );
    assert!(
        cleanup
            .function
            .blocks
            .iter()
            .any(|block| block.terminator == SourceTerminator::CleanupReturn)
    );
    assert!(
        !cleanup
            .function
            .blocks
            .iter()
            .any(|block| matches!(block.terminator, SourceTerminator::Return(_)))
    );
    assert!(
        cleanup
            .function
            .locals
            .iter()
            .any(|local| local.name == "own" && !local.parameter)
    );
    for inst in cleanup
        .function
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
    {
        if let SourceInst::ClearScopeRoots { locals } = inst {
            assert!(
                locals
                    .iter()
                    .all(|id| !cleanup.function.locals[id.0 as usize].parameter)
            );
        }
    }
}

#[test]
fn expression_cleanup_region_captures_registration_time_operand() {
    let program = lir("fn f() { let mut value = 1; defer println(value); value = 2; }");
    let outer = func(&program, "f");
    let cleanup = outer
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .find_map(|inst| {
            if let SourceInst::Defer { body, .. } = inst {
                Some(body)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(cleanup.captures.len(), 1);
    let captured = &outer.locals[cleanup.captures[0].0 as usize];
    assert!(captured.synthetic);
    assert_ne!(captured.name, "value");
}

// 1. a straight-line body is a single entry block
#[test]
fn l01_straight_line_single_block() {
    let p = lir("fn f() { let a = 1; print(a); }");
    let f = func(&p, "f");
    assert_eq!(f.blocks[0].instrs.len(), 3);
    assert!(matches!(
        &f.blocks[0].instrs[1],
        SourceInst::Compute {
            value: LirRvalue::Print { .. },
            ..
        }
    ));
    assert_eq!(f.blocks[0].terminator, SourceTerminator::Return(None));
}

// 2. entry block is always id 0
#[test]
fn l02_entry_is_block_zero() {
    let p = lir("fn f() { }");
    assert_eq!(func(&p, "f").blocks[0].id, BlockId(0));
}

// 3. an explicit `return v;` becomes a Return terminator with the value
#[test]
fn l03_return_value_terminator() {
    let p = lir("fn f() -> i64 { return 7; }");
    let f = func(&p, "f");
    assert!(matches!(
        &f.blocks[0].terminator,
        SourceTerminator::Return(Some(v)) if matches!(v.kind, HirExprKind::Int(7))
    ));
}

// 4. an empty function still gets an implicit `return`
#[test]
fn l04_empty_fn_implicit_return() {
    let p = lir("fn f() { }");
    assert_eq!(
        func(&p, "f").blocks[0].terminator,
        SourceTerminator::Return(None)
    );
}

// 5. `if` without else: entry branches then/merge, then jumps to merge
#[test]
fn l05_if_without_else_shape() {
    let p = lir("fn f(c: bool) { if c { print(1); } print(2); }");
    let f = func(&p, "f");
    let SourceTerminator::Branch {
        then_block,
        else_block,
        ..
    } = &f.blocks[0].terminator
    else {
        panic!("entry must branch");
    };
    // No else → the false edge goes straight to the merge block.
    assert_eq!(
        f.blocks[then_block.0].terminator,
        SourceTerminator::Jump(*else_block)
    );
    // The merge block holds the trailing statement.
    assert!(f.blocks[else_block.0].instrs.iter().any(|inst| matches!(
        inst,
        SourceInst::Compute {
            value: LirRvalue::Print {
                value: LirOperand::Int(2),
                ..
            },
            ..
        }
    )));
}

// 6. `if`/`else`: both arms jump to the same merge block
#[test]
fn l06_if_else_merges() {
    let p = lir("fn f(c: bool) { if c { print(1); } else { print(2); } print(3); }");
    let f = func(&p, "f");
    let SourceTerminator::Branch {
        then_block,
        else_block,
        ..
    } = &f.blocks[0].terminator
    else {
        panic!("entry must branch");
    };
    let SourceTerminator::Jump(merge_a) = f.blocks[then_block.0].terminator else {
        panic!("then must jump to merge");
    };
    let SourceTerminator::Jump(merge_b) = f.blocks[else_block.0].terminator else {
        panic!("else must jump to merge");
    };
    assert_eq!(merge_a, merge_b);
    assert_ne!(merge_a, *then_block);
    assert_ne!(merge_a, *else_block);
}

// 7. the branch condition is the lowered Bool expression
#[test]
fn l07_branch_cond_is_bool() {
    let p = lir("fn f(a: i64) { if a > 0 { print(1); } }");
    let f = func(&p, "f");
    let SourceTerminator::Branch { cond, .. } = &f.blocks[0].terminator else {
        panic!("entry must branch");
    };
    assert_eq!(cond.ty, Type::Bool);
}

// 8. `while`: entry jumps to a header that branches body/exit
#[test]
fn l08_while_header_shape() {
    let p = lir("fn f(c: bool) { while c { print(1); } }");
    let f = func(&p, "f");
    let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
        panic!("entry must jump to the loop header");
    };
    let SourceTerminator::Branch {
        then_block: body, ..
    } = &f.blocks[header.0].terminator
    else {
        panic!("header must branch");
    };
    // The body jumps back to the header (the loop backedge).
    assert_eq!(f.blocks[body.0].terminator, SourceTerminator::Jump(header));
}

// 9. the `while` condition lives in the header, not the entry block
#[test]
fn l09_while_cond_in_header() {
    let p = lir("fn f(a: i64) { while a > 0 { print(1); } }");
    let f = func(&p, "f");
    assert!(matches!(f.blocks[0].terminator, SourceTerminator::Jump(_)));
    let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
        unreachable!()
    };
    assert!(matches!(
        f.blocks[header.0].terminator,
        SourceTerminator::Branch { .. }
    ));
}

// 10. range-for desugars to induction let + bound let + header branch
#[test]
fn l10_range_for_desugar() {
    let p = lir("fn f() { for i in 0..3 { print(i); } }");
    let f = func(&p, "f");
    let names: Vec<_> = f.blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            SourceInst::Let { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"__for0_i"), "{names:?}");
    assert!(names.contains(&"__for0_end"), "{names:?}");
}

// 11. the range-for body rebinds the loop variable and increments
#[test]
fn l11_range_for_body_binding_and_increment() {
    let p = lir("fn f() { for i in 0..3 { print(i); } }");
    let f = func(&p, "f");
    let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
        unreachable!()
    };
    let SourceTerminator::Branch {
        then_block: body, ..
    } = &f.blocks[header.0].terminator
    else {
        panic!("header must branch");
    };
    let body = &f.blocks[body.0];
    assert!(matches!(
        &body.instrs[0],
        SourceInst::Let { name, .. } if name == "i"
    ));
    // The increment lives in a dedicated block (the `continue` target,
    // string-content comparison): body jumps to it, and it assigns the induction var.
    let SourceTerminator::Jump(inc) = body.terminator else {
        panic!("body must jump to the increment block");
    };
    let inc = &f.blocks[inc.0];
    assert!(matches!(
        inc.instrs.last(),
        Some(SourceInst::Assign { name, .. }) if name == "__for0_i"
    ));
    assert!(matches!(inc.terminator, SourceTerminator::Jump(h) if h == header));
}

// 12. array-for desugars to arr/index lets, a header that RE-READS `len()`
// (so growth/shrinkage inside the body is observed), and an indexed element
// bind
#[test]
fn l12_array_for_desugar() {
    let p = lir("fn f() { let xs = [1, 2]; for v in xs { print(v); } }");
    let f = func(&p, "f");
    let names: Vec<_> = f.blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            SourceInst::Let { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"__for0_arr"), "{names:?}");
    assert!(names.contains(&"__for0_i"), "{names:?}");
    // The length is NOT hoisted into a `let`.
    assert!(!names.contains(&"__for0_len"), "{names:?}");
    let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
        unreachable!()
    };
    let SourceTerminator::Branch {
        cond,
        then_block: body,
        ..
    } = &f.blocks[header.0].terminator
    else {
        panic!("header must branch");
    };
    // The bound is a fresh `__for0_arr.len()` call in the header itself.
    let HirExprKind::Var(condition) = &cond.kind else {
        panic!("header condition must be a flat value");
    };
    let header_instructions = &f.blocks[header.0].instrs;
    assert!(header_instructions.iter().any(|inst| matches!(inst,
            SourceInst::Compute { value: LirRvalue::IntrinsicCall { method, .. }, .. } if method == "__abi_len")));
    assert!(header_instructions.iter().any(|inst| matches!(inst,
            SourceInst::Compute { local, value: LirRvalue::Binary { op: crate::parser::ast::BinOp::Lt, .. }, .. }
                if f.locals[local.0 as usize].name == *condition)));
    // v = __for0_arr[__for0_i], typed with the element type.
    let (name, value) = f.blocks[body.0]
        .instrs
        .iter()
        .find_map(|inst| match inst {
            SourceInst::Let { name, value, .. } if name == "v" => Some((name, value)),
            _ => None,
        })
        .expect("body binds the loop variable");
    assert_eq!(name, "v");
    let HirExprKind::Var(index_result) = &value.kind else {
        panic!("index must be a flat value");
    };
    assert!(f.blocks[body.0].instrs.iter().any(|inst| matches!(inst,
            SourceInst::Compute { local, value: LirRvalue::Index { .. }, .. } if f.locals[local.0 as usize].name == *index_result)));
    assert_eq!(value.ty, Type::I64);
}

// 13. nested `for` loops get distinct induction variables
#[test]
fn l13_nested_for_unique_induction_vars() {
    let p = lir("fn f() { for i in 0..2 { for j in 0..2 { print(i + j); } } }");
    let f = func(&p, "f");
    let all_lets: Vec<String> = f
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter_map(|i| match i {
            SourceInst::Let { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(all_lets.iter().any(|n| n == "__for0_i"), "{all_lets:?}");
    assert!(all_lets.iter().any(|n| n == "__for1_i"), "{all_lets:?}");
}

// 14. a return inside an if leaves both paths terminated
#[test]
fn l14_return_inside_if() {
    let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
    let f = func(&p, "f");
    // Every block has a terminator (no panics, no fallthrough corruption).
    for b in &f.blocks {
        match &b.terminator {
            SourceTerminator::Jump(_)
            | SourceTerminator::Branch { .. }
            | SourceTerminator::Suspend { .. }
            | SourceTerminator::Return(_)
            | SourceTerminator::CleanupReturn => {}
        }
    }
    // The then-arm's return survives as a Return terminator.
    let SourceTerminator::Branch { then_block, .. } = &f.blocks[0].terminator else {
        panic!("entry must branch");
    };
    assert!(matches!(
        f.blocks[then_block.0].terminator,
        SourceTerminator::Return(Some(_))
    ));
}

// 15. statement order within a block is preserved
#[test]
fn l15_instr_order_preserved() {
    let p = lir("fn f() { let a = 1; let b = 2; print(a + b); }");
    let f = func(&p, "f");
    let kinds: Vec<_> = f.blocks[0]
        .instrs
        .iter()
        .map(|i| match i {
            SourceInst::Let { name, .. } => format!("let {name}"),
            SourceInst::Compute {
                value: LirRvalue::Binary { .. },
                ..
            } => "binary".to_string(),
            SourceInst::Compute {
                value: LirRvalue::Print { .. },
                ..
            } => "print".to_string(),
            SourceInst::Expr(_) => "expr".to_string(),
            SourceInst::ClearScopeRoots { .. } => "clear roots".to_string(),
            _ => "other".to_string(),
        })
        .collect();
    assert_eq!(kinds, ["let a", "let b", "binary", "print", "clear roots"]);
}

// 16. field/index/static assignments lower to their instructions
#[test]
fn l16_assignment_instructions() {
    let p = lir("class C { x: i64; static mut t: i64 = 0; } \
             fn f() { let p = new C(1); p.x = 2; let xs = [1]; xs[0] = 9; C::t = 5; }");
    let f = func(&p, "f");
    let instrs = &f.blocks[0].instrs;
    assert!(instrs.iter().any(|i| matches!(
        i,
        SourceInst::Compute {
            value: LirRvalue::FieldStore { .. },
            ..
        }
    )));
    assert!(instrs.iter().any(|i| matches!(
        i,
        SourceInst::Compute {
            value: LirRvalue::ArrayStore { .. },
            ..
        }
    )));
    assert!(instrs.iter().any(|i| matches!(
        i,
        SourceInst::Compute {
            value: LirRvalue::StaticStore { .. },
            ..
        }
    )));
}

// 17. class methods are flattened as `Class::method`
#[test]
fn l17_class_methods_flattened() {
    let p = lir("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
    assert!(
        p.functions
            .iter()
            .any(|f| f.name == FunctionId::method(TypeId::local("Box"), "get"))
    );
}

// 18. super.init becomes an explicit base constructor call.
#[test]
fn l18_constructor_flattened_with_super_init() {
    let p = lir(
        "open class A { v: i64; init(self, v: i64) { self.v = v; } } \
             class B extends A { init(self, v: i64) { super.init(v); } }",
    );
    let init = func(&p, "B::init");
    assert!(
            init.blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, SourceInst::Compute { value: LirRvalue::ConstructorCall { class, .. }, .. } if *class == TypeId::local("A")))
        );
}

// 19. params and return type are carried onto the LIR function
#[test]
fn l19_signature_carried() {
    let p = lir("fn f(a: i64, b: bool) -> i64 { return a; }");
    let f = func(&p, "f");
    assert_eq!(f.params.len(), 2);
    assert_eq!(f.return_type, Type::I64);
}

// 20. nested if inside while keeps the loop backedge intact
#[test]
fn l20_if_inside_while() {
    let p =
        lir("fn f(n: i64) { let mut i = 0; while i < n { if i > 2 { print(i); } i = i + 1; } }");
    let f = func(&p, "f");
    let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
        unreachable!()
    };
    // Some block jumps back to the header — the loop backedge survives the
    // nested if's merge.
    let backedges = f
        .blocks
        .iter()
        .filter(|b| b.id != BlockId(0) && b.terminator == SourceTerminator::Jump(header))
        .count();
    assert!(backedges >= 1);
}

// 21. the LIR text dump renders labeled blocks and terminators
#[test]
fn l21_text_dump_shape() {
    let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
    let text = format_source_program(&p);
    assert!(text.contains("bb0:"), "{text}");
    assert!(text.contains("branch c: bool bb"), "{text}");
    assert!(text.contains("return 1: i64"), "{text}");
}

// 22. expression-level control flow (ternary/match) stays in instructions
#[test]
fn l22_expression_control_flow_becomes_blocks() {
    let p = lir("fn f(c: bool) -> i64 { return c ? 1 : 2; }");
    let f = func(&p, "f");
    assert!(matches!(
        &f.blocks[0].terminator,
        SourceTerminator::Branch { .. }
    ));
    assert!(f.blocks.iter().any(|block| matches!(&block.terminator,
            SourceTerminator::Return(Some(v)) if matches!(v.kind, HirExprKind::Var(_)))));
}

#[test]
fn l26_async_await_is_an_explicit_suspend_edge() {
    let p = lir("async fn f() { await sleep(1); print(2); }");
    let f = func(&p, "f");
    assert!(f.is_async);
    assert!(f.blocks.iter().any(|block| matches!(
        block.terminator,
        SourceTerminator::Suspend {
            operation: SuspendOp::Sleep { .. },
            ..
        }
    )));
}

#[test]
fn l27_synthetic_locals_have_identity_without_source_spans() {
    let p = lir_checked("async fn f(xs: Array<i64>) { for x in xs { await yield(); print(x); } }");
    let f = func(&p, "f");
    let synthetic: Vec<_> = f.locals.iter().filter(|local| local.synthetic).collect();
    assert!(!synthetic.is_empty());
    assert!(synthetic.iter().all(|local| local.source_span.is_none()));
    let ids: std::collections::HashSet<_> = synthetic.iter().map(|local| local.id).collect();
    assert_eq!(ids.len(), synthetic.len());
    assert!(
        synthetic.iter().any(|local| {
            local.name.starts_with("__for") && f.async_frame.slot(local.id).is_some()
        }),
        "locals/frame: {:#?} / {:#?}",
        f.locals,
        f.async_frame
    );
}

#[test]
fn l28_async_frame_is_keyed_by_local_id_not_span() {
    let p = lir("async fn f() { let keep = 1; await yield(); print(keep); await yield(); }");
    let f = func(&p, "f");
    let keep = f.locals.iter().find(|local| local.name == "keep").unwrap();
    assert!(f.async_frame.locals.contains_key(&keep.id));
    assert!(
        f.async_frame
            .locals
            .keys()
            .all(|id| f.locals.get(id.0 as usize).is_some())
    );
}

#[test]
fn select_cleanup_graph_is_shared_across_all_winners() {
    for asynchronous in [false, true] {
        for count in [1, 10, 40] {
            let prefix = if asynchronous { "async " } else { "" };
            let mut source = format!("{prefix}fn f(ch: Channel<i64>) {{ select {{");
            for _ in 0..count {
                source.push_str("let value = ch.recv() => { print(value); }");
            }
            source.push_str("default => { print(0); } } }");
            let p = lir(&source);
            let f = func(&p, "f");
            let cleanups = f
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter_map(|inst| match inst {
                    SourceInst::SelectUnregister { operations, winner } => {
                        Some((operations, winner))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            // One quadratic alias-check graph, not one per winning arm.
            assert_eq!(cleanups.len(), 1, "{count} arms, async={asynchronous}");
            assert_eq!(cleanups[0].0.len(), count + 1);
            let picks = f
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter_map(|inst| match inst {
                    SourceInst::SelectPick { chosen, .. } => Some(chosen),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(picks, vec![cleanups[0].1]);
        }
    }
}

#[test]
fn l29_async_select_is_cfg_plus_suspend() {
    let p = lir(r#"
async fn f(ch: Channel<i64>) {
    select {
        let v = ch.recv() => { await yield(); print(v); }
        sleep(1) => { print(0); }
    }
}
"#);
    let f = func(&p, "f");
    assert!(f.blocks.iter().any(|block| {
        block
            .instrs
            .iter()
            .any(|inst| matches!(inst, SourceInst::SelectProbe { .. }))
    }));
    assert!(f.blocks.iter().any(|block| {
        block
            .instrs
            .iter()
            .any(|inst| matches!(inst, SourceInst::SelectPick { .. }))
    }));
    assert!(f.blocks.iter().any(|block| {
        block
            .instrs
            .iter()
            .any(|inst| matches!(inst, SourceInst::SelectUnregister { .. }))
    }));
    assert!(f.blocks.iter().any(|block| {
        block
            .instrs
            .iter()
            .any(|inst| matches!(inst, SourceInst::SelectCommit { .. }))
    }));
    assert!(f.blocks.iter().any(|block| matches!(
        block.terminator,
        SourceTerminator::Suspend {
            operation: SuspendOp::SelectWait { .. },
            ..
        }
    )));
    assert!(f.blocks.iter().any(|block| matches!(
        block.terminator,
        SourceTerminator::Suspend {
            operation: SuspendOp::Yield,
            ..
        }
    )));
}

#[test]
fn l30_nested_await_return_value_is_fixed_before_defer_flush() {
    let p = lir(r#"
async fn one() -> i64 { return 1; }
async fn f() -> i64 {
    let mut x = 1;
    defer { x = 9; }
    return (await one()) + x;
}
"#);
    let f = func(&p, "f");
    let block = f
        .blocks
        .iter()
        .find(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, SourceInst::FlushDefers { .. }))
        })
        .expect("return block must flush its defer");
    let flush_index = block
        .instrs
        .iter()
        .position(|inst| matches!(inst, SourceInst::FlushDefers { .. }))
        .unwrap();
    assert!(block.instrs[..flush_index].iter().any(|inst| matches!(
        inst,
        SourceInst::Compute {
            value: LirRvalue::Binary { .. },
            ..
        }
    )));
    let (return_local, return_name) = block.instrs[..flush_index]
        .iter()
        .find_map(|inst| match inst {
            SourceInst::Let {
                local, name, value, ..
            } if name.starts_with("__async_return_")
                && matches!(value.kind, HirExprKind::Var(_)) =>
            {
                Some((*local, name.as_str()))
            }
            _ => None,
        })
        .expect("the complete return expression must be stored before flushing defers");
    assert!(matches!(
        &block.terminator,
        SourceTerminator::Return(Some(HirExpr {
            kind: HirExprKind::Var(name),
            ..
        })) if name == return_name
    ));
    assert_eq!(f.locals[return_local.0 as usize].name, return_name);
}

/// The blocks a `match` arm's suspension is cut into.
/// A `match` whose arms do not suspend stays a single `Expr`/`Assign`
/// instruction, so every assertion here is also a check that the split is
/// taken only when an arm needs it.
#[test]
fn l31_suspending_match_arm_becomes_blocks() {
    let p = lir(r#"
async fn leaf(n: i64) -> i64 { return n; }
async fn f(which: i64) -> i64 {
    match which {
        1 => { return await leaf(10); }
        _ => { return await leaf(20); }
    }
}
"#);
    let f = func(&p, "f");
    // One test per non-catch-all arm, dispatched by a two-way branch.
    assert_eq!(
        f.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter(|inst| matches!(inst, SourceInst::MatchTest { .. }))
            .count(),
        1
    );
    assert!(
        f.blocks
            .iter()
            .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. }))
    );
    // Both arms suspend, so each one ends up behind its own suspend edge.
    assert_eq!(
        f.blocks
            .iter()
            .filter(|block| matches!(
                block.terminator,
                SourceTerminator::Suspend {
                    operation: SuspendOp::AwaitTask { .. },
                    ..
                }
            ))
            .count(),
        2
    );
}

/// The dispatch chain reads one scrutinee local, evaluated once before the
/// first test. Re-reading the source expression per arm would run its side
/// effects once per test.
#[test]
fn l32_match_scrutinee_is_evaluated_once() {
    let p = lir(r#"
async fn leaf(n: i64) -> i64 { return n; }
async fn f(which: i64) -> i64 {
    match which + 1 {
        1 => { return await leaf(10); }
        2 => { return await leaf(20); }
        _ => { return 0; }
    }
}
"#);
    let f = func(&p, "f");
    let tests: Vec<_> = f
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter_map(|inst| match inst {
            SourceInst::MatchTest { scrutinee, .. } => Some(*scrutinee),
            _ => None,
        })
        .collect();
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[0], tests[1]);
    let scrutinee = f.locals[tests[0].0 as usize].clone();
    assert!(scrutinee.synthetic);
    assert!(scrutinee.name.starts_with("__async_match_scrutinee_"));
}

/// A catch-all arm cannot fail, so it is entered by a jump with no test at
/// all -- and nothing is lowered after it, since no later arm is reachable.
#[test]
fn l33_catch_all_arm_needs_no_test() {
    let p = lir(r#"
async fn leaf(n: i64) -> i64 { return n; }
async fn f(which: i64) -> i64 {
    match which {
        bound => { return await leaf(bound); }
    }
}
"#);
    let f = func(&p, "f");
    assert!(
        !f.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|inst| matches!(inst, SourceInst::MatchTest { .. }))
    );
    assert_eq!(f.blocks[0].terminator, SourceTerminator::Jump(BlockId(1)));
    // The catch-all still binds the whole scrutinee.
    assert!(
        f.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|inst| matches!(inst, SourceInst::MatchBind { .. }))
    );
}

/// A payload binding is destructured inside the arm's own block, so it is
/// defined on exactly the path that reads it -- which is what lets liveness
/// frame it for one arm without framing it for the others.
#[test]
fn l34_arm_bindings_are_defined_in_the_arm_block() {
    let p = lir(r#"
enum Shape { Circle(i64), Rect(i64, i64), Empty }
async fn leaf(n: i64) -> i64 { return n; }
async fn f(shape: Shape) -> i64 {
    match shape {
        Shape::Circle(r) => { let n = r + 1; await yield(); return n; }
        Shape::Rect(w, h) => { let a = await leaf(w); return a + h; }
        Shape::Empty => { return 0; }
    }
}
"#);
    let f = func(&p, "f");
    let binds: Vec<_> = f
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter_map(|inst| match inst {
            SourceInst::MatchBind { bindings, .. } => Some(bindings.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(binds.len(), 2, "only the two payload arms bind");
    assert_eq!(binds[0].len(), 1);
    assert_eq!(binds[1].len(), 2);
    // `h` is read after its arm's suspension, so it must be framed; `r` is
    // dead by the time its arm suspends and must not be. Both bindings come
    // out of the same instruction shape, so what separates them is which
    // block each was defined in.
    let local = |name: &str| f.locals.iter().find(|l| l.name == name).unwrap().id;
    assert!(f.async_frame.slot(local("h")).is_some());
    assert!(f.async_frame.slot(local("r")).is_none());
}

#[test]
fn synchronous_root_conditionals_are_cfg() {
    for source in [
        "fn f(c: bool) -> i64 { return c ? 10 : 20; }",
        "fn f(c: bool, d: bool) -> bool { return c && d; }",
        "fn f(c: bool, d: bool) -> bool { return c || d; }",
        "fn f(c: bool, d: bool) -> i64 { if c && d { return 10; } return 20; }",
        "fn f(c: bool) -> i64 { let v = c ? 10 : 20; return v; }",
    ] {
        let p = lir(source);
        let f = func(&p, "f");
        assert!(
            f.blocks
                .iter()
                .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. })),
            "{source}"
        );
        for block in &f.blocks {
            if let SourceTerminator::Return(Some(value))
            | SourceTerminator::Branch { cond: value, .. } = &block.terminator
            {
                assert!(
                    !matches!(
                        value.kind,
                        HirExprKind::Ternary { .. }
                            | HirExprKind::Binary {
                                op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                                ..
                            }
                    ),
                    "{source}"
                );
            }
        }
    }
}

#[test]
fn nested_conditionals_are_evaluated_before_eager_parents() {
    for source in [
        "fn f(c: bool, x: i64) -> i64 { return x + (c ? 10 : 20); }",
        "fn g(a: i64, b: i64) -> i64 { return a + b; } fn f(c: bool, x: i64) -> i64 { return g(x, c ? 10 : 20); }",
        "fn f(c: bool, xs: Array<i64>) -> i64 { return xs[c ? 0 : 1]; }",
        "fn f(c: bool, xs: Array<i64>) { xs[c ? 0 : 1] = c ? 10 : 20; }",
        "fn g(a: &mut i64, b: i64) { a = b; } fn f(c: bool, xs: Array<i64>) { g(&xs[c ? 0 : 1], c ? 10 : 20); }",
    ] {
        let p = lir_checked(source);
        let f = func(&p, "f");
        assert!(
            f.blocks
                .iter()
                .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. })),
            "{source}"
        );
        let check = |value: &HirExpr| assert!(!expression_needs_cfg(value), "{source}: {value:?}");
        for block in &f.blocks {
            for inst in &block.instrs {
                match inst {
                    SourceInst::Let { value, .. }
                    | SourceInst::Assign { value, .. }
                    | SourceInst::Expr(value) => check(value),
                    SourceInst::IndexAssign {
                        array,
                        index,
                        value,
                    } => {
                        check(array);
                        check(index);
                        check(value);
                    }
                    _ => {}
                }
            }
            match &block.terminator {
                SourceTerminator::Return(Some(value))
                | SourceTerminator::Branch { cond: value, .. } => check(value),
                _ => {}
            }
        }
    }
}

#[test]
fn lifted_lambdas_exclusively_own_their_bodies() {
    let mut program = lir(
        "fn f() -> i64 { let apply = |x: i64| -> i64 { let next = |y: i64| y + 1; return next(x); }; return apply(10); }",
    );
    assert_eq!(program.lambdas.len(), 2);
    let mut constructions = 0;
    for function in program.functions.iter_mut().chain(
        program
            .lambdas
            .iter_mut()
            .map(|lambda| &mut lambda.function),
    ) {
        constructions += function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter(|inst| {
                matches!(
                    inst,
                    SourceInst::Compute {
                        value: LirRvalue::Closure { .. },
                        ..
                    }
                )
            })
            .count();
        function.visit_expr_roots_mut(|expr| {
            for node in expr.walk_postorder(true) {
                if let HirExprKind::Lambda { body, .. } = &node.kind {
                    constructions += 1;
                    assert!(body.is_empty());
                }
            }
        });
    }
    assert_eq!(constructions, 2);
}

/// Ordinary match arms also belong to the explicit control-flow graph.
#[test]
fn l35_non_suspending_match_is_split() {
    let p = lir(r#"
async fn f(which: i64) -> i64 {
    let mut out = 0;
    match which {
        1 => { out = 10; }
        _ => { out = 20; }
    }
    await yield();
    return out;
}
"#);
    let f = func(&p, "f");
    assert!(
        f.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|inst| matches!(
                inst,
                SourceInst::MatchTest { .. } | SourceInst::MatchBind { .. }
            ))
    );
}

// 37. a `lock` body and its continuation are emitted BEFORE the block
// that holds the `let` they read.
//
// This is the shape that made lazy binding at `SourceInst::Let` wrong: the
// assignment in the section and the read after it both land in blocks of
// lower index than the `let`, so codegen reaches them first. The backend
// answers it by binding every local at function entry; this pins the
// premise, so a lowering change that reorders the blocks shows up here
// rather than as a silent dependency the backend no longer needs.
#[test]
fn l37_lock_body_is_emitted_before_the_let_it_reads() {
    // The call is what makes the shape: an async body is split at its
    // preemption points and each continuation is APPENDED, so the block
    // holding everything after the `gc_collect()` — the `let` included —
    // is allocated after the section's own blocks.
    let p = lir(r#"
async fn peek(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut got = 0;
    lock m as value { got = value; }
    return got;
}
"#);
    let f = func(&p, "peek");
    let block_of = |pick: &dyn Fn(&SourceInst) -> bool| {
        f.blocks
            .iter()
            .position(|block| block.instrs.iter().any(pick))
            .expect("no block holds the instruction")
    };
    let binds = block_of(&|inst| matches!(inst, SourceInst::Let { name, .. } if name == "got"));
    let writes = block_of(&|inst| matches!(inst, SourceInst::Assign { name, .. } if name == "got"));
    assert!(
        writes < binds,
        "the section writes `got` in bb{writes} and binds it in bb{binds}; \
             the binding is no longer the later block"
    );
}

fn lock_exit_events(function: &SourceFunction, block: &SourceBlock) -> Vec<String> {
    block
        .instrs
        .iter()
        .filter_map(|inst| match inst {
            SourceInst::FlushDefers { scopes, .. } => Some(format!("flush:{scopes}")),
            SourceInst::ReleaseLock(slots) => Some(format!(
                "release:{}",
                function.locals[slots.binding.0 as usize].name
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn nested_lock_return_releases_all_sections_inside_out() {
    let program = lir(r#"
async fn nested(a: Mutex<i64>, b: Mutex<i64>, c: Mutex<i64>) -> i64 {
    defer println(0);
    lock a as outer {
        defer println(1);
        lock b as middle {
            defer println(2);
            lock c as inner {
                defer println(3);
                return inner;
            }
        }
    }
}
"#);
    let function = func(&program, "nested");
    let returning = function
        .blocks
        .iter()
        .find(|block| matches!(block.terminator, SourceTerminator::Return(Some(_))))
        .expect("explicit return");
    assert_eq!(
        lock_exit_events(function, returning),
        [
            "flush:1",
            "release:inner",
            "flush:1",
            "release:middle",
            "flush:1",
            "release:outer",
            "flush:1",
        ]
    );
}

#[test]
fn nested_lock_loop_exits_release_only_departing_sections() {
    for exit in ["break", "continue"] {
        let program = lir(&format!(
            r#"
async fn nested(a: Mutex<i64>, b: Mutex<i64>, c: Mutex<i64>) {{
    lock a as outer {{
        while true {{
            lock b as middle {{
                lock c as inner {{ {exit}; }}
            }}
        }}
    }}
}}
"#
        ));
        let function = func(&program, "nested");
        let exiting = function
            .blocks
            .iter()
            .find(|block| {
                let events = lock_exit_events(function, block);
                events.iter().any(|event| event == "release:inner")
            })
            .expect("loop exit cleanup");
        assert_eq!(
            lock_exit_events(function, exiting),
            ["flush:1", "release:inner", "flush:1", "release:middle"],
            "{exit} must retain the enclosing critical section"
        );
    }
}

// 38. the same in a synchronous body: `if/else` lowers the merge block
// before the `else` arm that jumps to it, so a block index has never been
// a position in an order control can flow in.
#[test]
fn l38_if_else_merge_is_emitted_before_its_predecessor() {
    let p = lir("fn f(c: bool) -> i64 { if c { return 1; } else { print(2); } return 3; }");
    let f = func(&p, "f");
    let SourceTerminator::Branch { else_block, .. } = &f.blocks[0].terminator else {
        panic!("entry does not branch");
    };
    let merge = f
        .blocks
        .iter()
        .position(|block| {
            matches!(&block.terminator, SourceTerminator::Return(Some(v))
                    if matches!(v.kind, HirExprKind::Int(3)))
        })
        .expect("no merge block");
    assert!(
        merge < else_block.0,
        "merge is bb{merge} and the else arm bb{}; index order no longer \
             puts a block before its predecessor",
        else_block.0
    );
}

/// Synchronous matches use the same dispatch graph as async matches.
#[test]
fn l36_sync_match_is_split() {
    let p = lir(r#"
fn f(which: i64) -> i64 {
    match which {
        1 => { return 10; }
        _ => { return 20; }
    }
}
"#);
    let f = func(&p, "f");
    assert!(
        f.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|inst| matches!(
                inst,
                SourceInst::MatchTest { .. } | SourceInst::MatchBind { .. }
            ))
    );
}
