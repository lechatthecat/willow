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
            let hir = Default::default();
            let resolution = LoweringDeclarations::new(&hir);
            let mut builder = Builder::new(&[], false, &resolution);
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
                            binding: HirCallBinding::Function,
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
                let declarations = Default::default();
                collect_lambdas_in_expr(
                    &expr,
                    &mut lambdas,
                    &LoweringDeclarations::new(&declarations),
                );
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
    lower_source_program(&checked_hir(src))
}

pub(super) fn checked_hir(src: &str) -> HirProgram {
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
    hir
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
fn deferred_callable_uses_the_binding_resolved_at_registration() {
    for count in [1, 8, 32] {
        let source = format!(
            "fn first(n: i64) {{}} fn second(n: i64) {{}} fn probe(stable: fn(i64) -> void, variable: &mut fn(i64) -> void) {{
                let f = first; {}
                if true {{ let mut f = first; defer f(1); f = second; }}
                defer stable(2); defer variable(3); variable = second;
            }}",
            "defer f(0);".repeat(count)
        );
        let program = lir_checked(&source);
        let outer = func(&program, "probe");
        let callees: Vec<_> = outer
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                let SourceInst::Defer { body, .. } = inst else {
                    return None;
                };
                let [callee, argument] = body.captures.as_slice() else {
                    panic!("one callee and one frozen argument");
                };
                assert!(outer.locals[argument.0 as usize].synthetic);
                Some(&outer.locals[callee.0 as usize])
            })
            .collect();
        assert_eq!(callees.len(), count + 3);
        assert!(
            callees[..count]
                .iter()
                .all(|local| !local.synthetic && local.name == "f")
        );
        assert!(callees[count].synthetic);
        assert_eq!(callees[count + 1].name, "stable");
        assert!(callees[count + 2].synthetic);
        assert_eq!(
            outer
                .locals
                .iter()
                .filter(|l| l.synthetic && l.name.contains("_callee"))
                .count(),
            2
        );
    }
}

#[test]
fn deferred_callable_stability_ends_at_its_lexical_cleanup() {
    for count in [1, 8, 32] {
        let source = format!(
            "fn one() {{}} fn two() {{}} fn probe(view: & fn()->void) {{
                let mut f = one;
                if true {{ {} }}
                {} f = two; {}
                defer view();
            }}",
            "defer f();".repeat(count),
            "defer f();".repeat(count),
            "defer f();".repeat(count)
        );
        let program = lir_checked(&source);
        let outer = func(&program, "probe");
        let slots: Vec<_> = outer
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                SourceInst::Defer { body, .. } => {
                    let [callee] = body.captures.as_slice() else {
                        panic!("one callee input")
                    };
                    Some(&outer.locals[callee.0 as usize])
                }
                _ => None,
            })
            .collect();
        assert_eq!(slots.len(), 3 * count + 1);
        assert!(slots[..count].iter().all(|slot| slot.name == "f"));
        assert!(slots[count..2 * count].iter().all(|slot| slot.synthetic));
        assert!(
            slots[2 * count..3 * count]
                .iter()
                .all(|slot| slot.name == "f")
        );
        assert_eq!(slots.last().unwrap().name, "view");
    }
}

#[test]
fn skipped_writes_and_read_only_borrows_keep_a_deferred_callable_in_its_slot() {
    let source = "fn first() {} fn second() {} fn inspect(g: & fn()->void) { g(); }
        fn replace(g: &mut fn()->void)->bool { g = second; return true; }
        fn probe(flag: bool) {
            let mut f: fn()->void = first;
            defer f();
            inspect(&f);
            if false { f = second; }
            while false { replace(&f); }
            let a = false && replace(&f);
            let b = true || replace(&f);
            let c = flag ? true : (false ? replace(&f) : true);
        }";
    let program = lir_checked(source);
    let outer = func(&program, "probe");
    let capture = outer
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .find_map(|inst| match inst {
            SourceInst::Defer { body, .. } => Some(&body.captures),
            _ => None,
        })
        .expect("cleanup registration");
    assert_eq!(capture.len(), 1);
    assert_eq!(outer.locals[capture[0].0 as usize].name, "f");
}

#[test]
fn deferred_callable_stability_scan_shares_writes_across_registrations() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    for mutable in [false, true] {
        for registrations in [1, 8, 32] {
            for assignments in [1, 8, 32] {
                let source = format!(
                    "fn one() {{}} fn probe() {{
                    let {} f = one; let mut x = 0; {} {} }}",
                    if mutable { "mut" } else { "" },
                    "defer f();".repeat(registrations),
                    "x = x + 1;".repeat(assignments)
                );
                SCAN_COUNTS.set(Default::default());
                let _ = checked_hir(&source);
                let counts = SCAN_COUNTS.get();
                println!(
                    "mutable={mutable} registrations={registrations} assignments={assignments} {counts:?}"
                );
                if mutable {
                    assert_eq!(counts.watched_slots, 1);
                    assert_eq!(counts.work_items, 4 * assignments + 2 * registrations + 1);
                    assert_eq!(counts.reachability_checks, assignments + registrations + 2);
                    assert!(counts.peak_pending <= 4);
                } else {
                    assert_eq!(counts, Default::default());
                }
            }
        }
    }
}

#[test]
fn deferred_callable_stability_streams_wide_calls_and_arrays() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    for width in [2, 16, 128, 512] {
        let args = vec!["1"; width].join(",");
        let parameters = (0..width)
            .map(|i| format!("p{i}: i64"))
            .collect::<Vec<_>>()
            .join(",");
        for tail in [format!("consume({args});"), format!("let xs = [{args}];")] {
            let source = format!(
                "fn one() {{}} fn consume({parameters}) {{}} fn probe() {{
                let mut f = one; defer f(); {tail} }}"
            );
            let _ = checked_hir(&source);
            let counts = SCAN_COUNTS.get();
            println!("width={width} array={} {counts:?}", tail.starts_with("let"));
            assert_eq!(counts.watched_slots, 1);
            assert!(counts.work_items >= width);
            assert!(counts.peak_pending <= 3);
        }
    }
}

#[test]
fn deferred_callable_stability_uses_each_reference_arguments_declared_mode() {
    let program = lir_checked(
        "fn one() {} fn two() {}
        fn inspect(read: & fn()->void, middle: i64, write: &mut fn()->void) { write = two; }
        fn probe() { let mut left: fn()->void = one; let mut right: fn()->void = one;
            defer left(); defer right(); inspect(&left, 1, &right); }",
    );
    let outer = func(&program, "probe");
    let captures = outer
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter_map(|i| match i {
            SourceInst::Defer { body, .. } => {
                let [slot] = body.captures.as_slice() else {
                    panic!("expected a single callee input")
                };
                Some(&outer.locals[slot.0 as usize])
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(captures.len(), 2);
    assert_eq!(captures[0].name, "left");
    assert!(captures[1].synthetic);
}

#[test]
fn deferred_callable_stability_borrows_the_parameter_index_only_for_watched_aliases() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    for width in [1, 8, 64, 256] {
        let parameters = (0..width)
            .map(|i| format!("p{i}: &mut fn()->void"))
            .collect::<Vec<_>>()
            .join(",");
        for borrowed in [false, true] {
            let target = if borrowed { "view" } else { "f" };
            let source = format!(
                "fn one() {{}} fn probe(view: & fn()->void, {parameters}) {{
                let mut f = one; defer {target}(); p0 = one; }}"
            );
            let program = lir_checked(&source);
            let counts = SCAN_COUNTS.get();
            let outer = func(&program, "probe");
            let capture = outer
                .blocks
                .iter()
                .flat_map(|b| &b.instrs)
                .find_map(|i| match i {
                    SourceInst::Defer { body, .. } => Some(body.captures.as_slice()),
                    _ => None,
                })
                .unwrap();
            let [id] = capture else {
                panic!("expected one captured callee")
            };
            assert_eq!(outer.locals[id.0 as usize].synthetic, borrowed);
            println!("ref_params={} borrowed={borrowed} {counts:?}", width + 1);
            assert_eq!(counts.watched_slots, 1);
            assert_eq!(counts.borrowed_parameter_index, borrowed);
            assert_eq!(counts.alias_lookups, if borrowed { 2 } else { 0 });
        }
    }
}

#[test]
fn deferred_callable_stability_counts_scope_reachability_before_a_final_registration() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    for prefix in [0, 1, 8, 64, 256] {
        let source = format!(
            "fn one() {{}} fn probe() {{ let mut f = one; {} defer f(); }}",
            "1;".repeat(prefix)
        );
        let _ = checked_hir(&source);
        let counts = SCAN_COUNTS.get();
        println!("prefix={prefix} {counts:?}");
        assert_eq!(counts.reachability_checks, prefix + 2);
        assert_eq!(counts.work_items, 3);
    }
}

#[test]
fn skipped_deferred_calls_do_not_extend_the_stability_scan_into_initializers() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    let mut total_operands = 0;
    for width in [1, 8, 64, 512] {
        let values = vec!["7"; width].join(",");
        for reassigned in [false, true] {
            let write = if reassigned { "f = two;" } else { "" };
            let program = lir_checked(&format!(
                "fn one() {{}} fn two() {{}} fn probe() {{
                 let mut f = one; let xs = [{values}]; defer f();
                 if false {{ defer f(); }} {write} }}"
            ));
            let counts = SCAN_COUNTS.get();
            let outer = func(&program, "probe");
            let [id] = only_cleanup(outer).captures.as_slice() else {
                panic!("expected one callee")
            };
            assert_eq!(outer.locals[id.0 as usize].synthetic, reassigned);
            println!("width={width} reassigned={reassigned} {counts:?}");
            total_operands += counts.integer_operands;
        }
    }
    assert_eq!(total_operands, 0);
}

#[test]
fn deferred_callable_stability_counts_only_reachable_registrations() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    // The diverging match arm allows a registration inside a boolean operand.
    let operand = "match flag { true => { defer f(); return; }, false => true }";
    let cases = [
        ("false branch", "if false { defer f(); }".into(), 0),
        ("true alternate", "if true {} else { defer f(); }".into(), 0),
        ("false loop", "while false { defer f(); }".into(), 0),
        ("return", "return; defer f();".into(), 0),
        (
            "return in taken branch",
            "if true { return; } defer f();".into(),
            0,
        ),
        (
            "return in both branches",
            "if flag { return; } else { return; } defer f();".into(),
            0,
        ),
        ("never", r#"panic("stop"); defer f();"#.into(), 0),
        ("break", "while flag { break; defer f(); }".into(), 0),
        ("continue", "while flag { continue; defer f(); }".into(), 0),
        ("and", format!("let result = NEVER && ({operand});"), 0),
        ("or", format!("let result = true || ({operand});"), 0),
        (
            "ternary",
            format!("let result = false ? ({operand}) : true;"),
            0,
        ),
        (
            "nested",
            format!("if false {{ let result = flag && ({operand}); defer f(); }}"),
            0,
        ),
        ("live branch", "if flag { defer f(); }".into(), 1),
        (
            "live alternate",
            "if false {} else { defer f(); }".into(),
            1,
        ),
        ("live loop", "while flag { defer f(); break; }".into(), 1),
        (
            "dynamic and",
            format!("let result = flag && ({operand});"),
            1,
        ),
        (
            "dynamic or",
            format!("let result = flag || ({operand});"),
            1,
        ),
        (
            "dynamic ternary",
            format!("let result = flag ? ({operand}) : ({operand});"),
            2,
        ),
        (
            "scope restoration",
            "if flag { return; } defer f();".into(),
            1,
        ),
    ];
    for (label, tail, extra_sites) in cases {
        for outer_registration in [false, true] {
            SCAN_COUNTS.set(Default::default());
            let first = if outer_registration { "defer f();" } else { "" };
            let _ = checked_hir(&format!(
                "const NEVER: bool = false; fn one() {{}}
                fn probe(flag: bool) {{ let mut f = one; let xs = [7,7,7,7]; {first} {tail} }}"
            ));
            let counts = SCAN_COUNTS.get();
            assert_eq!(
                counts.resolved_sites,
                usize::from(outer_registration) + extra_sites,
                "{label}"
            );
            assert_eq!(counts.integer_operands, 0, "{label}");
            if !outer_registration && extra_sites == 0 {
                assert_eq!(counts, Default::default(), "{label}");
            }
        }
    }
}

#[test]
fn guaranteed_branch_exits_avoid_deferred_callee_snapshots() {
    use crate::ir::lower::defer_stability::SCAN_COUNTS;

    for count in [1, 8, 32] {
        for (label, branch, falls_through) in [
            ("literal", "if true { return; }", false),
            ("alternate", "if false {} else { return; }", false),
            ("both", "if flag { return; } else { return; }", false),
            (
                "nested",
                "if flag { if true { return; } } else { return; }",
                false,
            ),
            ("dynamic", "if flag { return; }", true),
            ("nested dynamic", "if true { if flag { return; } }", true),
            (
                "recover",
                r#"if true { defer match recover() { Some(_) => {}, None => {} } panic("boom"); }"#,
                true,
            ),
        ] {
            let source = format!(
                "fn one() {{}} fn two() {{}} fn probe(flag: bool) {{
                let mut f = one; {} {branch} f = two; }}",
                "defer f();".repeat(count)
            );
            let program = lir_checked(&source);
            let resolved = SCAN_COUNTS.get().resolved_sites;
            let outer = func(&program, "probe");
            let snapshots = outer
                .locals
                .iter()
                .filter(|l| l.synthetic && l.name.contains("_callee"))
                .count();
            println!("{label} registrations={count} snapshots={snapshots} resolved={resolved}");
            assert_eq!(resolved, count, "{label}");
            assert_eq!(snapshots, if falls_through { count } else { 0 }, "{label}");
        }
    }
}

#[test]
fn mixed_defer_registrations_keep_cleanup_slots_linear() {
    for count in [1, 8, 32] {
        let source = format!(
            "fn first() {{}} fn second() {{}} fn probe() {{ let mut f = first; {} {} f = second; }}",
            "defer f();".repeat(count),
            "defer first();".repeat(count)
        );
        let program = lir_checked(&source);
        let sites: Vec<_> = func(&program, "probe")
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                SourceInst::Defer { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert_eq!(sites.len(), 2 * count);
        let (indirect, direct) = sites.split_at(count);
        let total: usize = sites.iter().map(|body| body.function.locals.len()).sum();
        println!("sites_per_group={count} cleanup_slots={total}");
        assert!(indirect.iter().all(|body| body.captures.len() == 1));
        assert!(direct.iter().all(|body| body.captures.is_empty()));
        assert_eq!(total, 4 * count);
    }
}

#[test]
fn mixed_defer_blocks_inherit_lexical_reads_and_writes_across_snapshots() {
    for count in [1, 8, 32] {
        let source = format!(
            "fn first() {{}} fn second() {{}} fn probe() {{ let mut dst = 0; let mut f = first; {}
             let src = 7; {} {} f = second; }}",
            "defer f();".repeat(count),
            "defer f();".repeat(count),
            "defer { dst = src; }".repeat(count),
        );
        let program = lir_checked(&source);
        let outer = func(&program, "probe");
        let sites = outer
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                if let SourceInst::Defer { body, .. } = inst {
                    Some(body)
                } else {
                    None
                }
            });
        let mut block_count = 0;
        let mut total = 0;
        for body in sites.skip(2 * count) {
            block_count += 1;
            total += body.function.locals.len();
            let names: Vec<_> = body
                .captures
                .iter()
                .map(|id| outer.locals[id.0 as usize].name.as_str())
                .collect();
            assert_eq!(names, ["dst", "src"]);
            assert!(
                body.function
                    .locals
                    .iter()
                    .all(|local| !local.synthetic || !local.parameter)
            );
        }
        assert_eq!(block_count, count);
        assert_eq!(total, 2 * count);
    }
}

#[test]
fn lexical_cleanup_imports_only_referenced_parent_slots() {
    fn sites(function: &SourceFunction) -> impl Iterator<Item = &SourceDeferBody> {
        function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                if let SourceInst::Defer { body, .. } = inst {
                    Some(body)
                } else {
                    None
                }
            })
    }
    for unrelated in [1, 8, 32] {
        let padding: String = (0..unrelated)
            .map(|i| format!("let unused{i} = {i};"))
            .collect();
        for count in [1, 8, 32] {
            let source = format!(
                "fn probe() {{ {padding} let mut dst = 0; {} {} }}",
                "defer { dst = 1; }".repeat(count),
                "defer { defer { dst = 2; } }".repeat(count)
            );
            defer_captures::VISITED_NODES.set(0);
            let program = lir_checked(&source);
            let visits = defer_captures::VISITED_NODES.get();
            assert_eq!(visits, 5 * count);
            let parent = func(&program, "probe");
            let dst = parent.locals.iter().find(|l| l.name == "dst").unwrap().id;
            assert_eq!(dst.0 as usize, unrelated);
            let mut total = 0;
            for (i, body) in sites(parent).enumerate() {
                assert_eq!(body.captures, [dst]);
                total += body.function.locals.len();
                if i >= count {
                    let inner = sites(&body.function).next().unwrap();
                    assert_eq!(inner.captures, [LirLocalId(0)]);
                    assert_eq!(body.function.locals[0].name, "dst");
                    total += inner.function.locals.len();
                }
            }
            println!(
                "unrelated={unrelated} block_pairs={count} all_region_slots={total} analyzed_nodes={visits}"
            );
            assert_eq!(total, 3 * count);
        }
    }
}

fn only_cleanup(parent: &SourceFunction) -> &SourceDeferBody {
    let mut sites = parent
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter_map(|inst| match inst {
            SourceInst::Defer { body, .. } => Some(body),
            _ => None,
        });
    let site = sites.next().expect("one cleanup registration");
    assert!(sites.next().is_none());
    site
}

#[test]
fn capture_free_cleanup_skips_local_and_cfg_discovery() {
    for width in [1, 8, 32] {
        let locals: String = (0..width).map(|i| format!("let mut v{i} = 0;")).collect();
        for assignments in [1, 8, 32] {
            let writes: String = (0..assignments)
                .map(|i| format!("v{} = {i};", i % width))
                .collect();
            let source = format!("fn probe() {{ defer {{ {locals} {writes} println(v0); }} }}");
            CAPTURE_DISCOVERY_VISITS.set(0);
            let program = lir_checked(&source);
            let site = only_cleanup(func(&program, "probe"));
            let slots = site.function.locals.len();
            let instructions: usize = site.function.blocks.iter().map(|b| b.instrs.len()).sum();
            assert!(site.captures.is_empty());
            assert!(slots >= width && instructions >= assignments);
            let count = CAPTURE_DISCOVERY_VISITS.get();
            println!(
                "locals={width} assignments={assignments} slots={slots} instructions={instructions} discovery_visits={count}"
            );
            assert_eq!(count, 0);
        }
    }
}

#[test]
fn one_cleanup_input_bounds_discovery_storage() {
    for locals in [1, 16, 128] {
        let declarations: String = (0..locals).map(|i| format!("let mut v{i} = 0;")).collect();
        for assignments in [1, 16, 128] {
            let writes: String = (0..assignments)
                .map(|i| format!("v{} = {i};", i % locals))
                .collect();
            // The only parent access comes last, so the private reads and writes are examined.
            let source = format!(
                "fn probe() {{ let mut dst = 0; defer {{ {declarations} {writes} dst = v0; }} }}"
            );
            CAPTURE_DISCOVERY_STORAGE.set((usize::MAX, usize::MAX));
            let program = lir_checked(&source);
            let site = only_cleanup(func(&program, "probe"));
            assert_eq!(site.captures, [LirLocalId(0)]);
            assert!(site.function.locals.len() > locals);
            let storage = CAPTURE_DISCOVERY_STORAGE.get();
            println!("locals={locals} assignments={assignments} discovery_storage={storage:?}");
            assert_eq!(storage, (1, 1), "one name and one bitmap word");
        }
    }
}

#[test]
fn sparse_cleanup_captures_preserve_ordered_parent_ids() {
    for width in [8, 32, 128] {
        let locals: String = (0..width)
            .map(|i| format!("let pad{i} = 0; let mut v{i} = 0;"))
            .collect();
        let live = [1, width / 2, width - 1];
        let writes: String = live.iter().rev().map(|i| format!("v{i} = 2;")).collect();
        let hir = checked_hir(&format!("fn probe() {{ {locals} defer {{ {writes} }} }}"));
        let f = hir
            .functions
            .iter()
            .find(|f| f.name.is_free_named("probe"))
            .unwrap();
        let (defer, declarations) = f.body.split_last().unwrap();
        let HirStmt::Defer {
            body: HirDeferBody::Block(stmts),
            ..
        } = defer
        else {
            panic!("expected a deferred block")
        };
        // Supply an explicit wide input table to the region builder. The body
        // only references three. This checks post-lowering filtering independently
        // of which inputs the lexical analysis requests.
        let resolution = LoweringDeclarations::new(&hir.resolution);
        let mut builder = Builder::new(&f.params, false, &resolution);
        builder.lower_stmts(declarations);
        let inputs: Vec<_> = (0..width)
            .map(|i| builder.local_by_name[&format!("v{i}")])
            .collect();
        CAPTURE_DISCOVERY_VISITS.set(0);
        let site = builder.lower_defer_region(stmts, false, &inputs, None);
        assert!(CAPTURE_DISCOVERY_VISITS.get() > 0);
        let imports: Vec<_> = site
            .function
            .locals
            .iter()
            .filter(|local| local.parameter)
            .collect();
        assert_eq!(imports.len(), width);
        for (index, local) in imports.into_iter().enumerate() {
            assert_eq!(local.id, LirLocalId(index as u32));
            assert_eq!(local.name, format!("v{index}"));
        }
        assert_eq!(site.captures, live.map(|i| LirLocalId((2 * i + 1) as u32)));
    }
}

#[test]
fn false_while_adds_no_cleanup_slots_at_any_depth() {
    for width in [0, 1, 8, 32] {
        let locals: String = (0..width).map(|i| format!("let mut v{i} = {i};")).collect();
        let skipped: String = (0..width)
            .map(|i| format!("v{i} = dst; dst = v{i};"))
            .collect();
        for depth in [1, 8, 32] {
            let source = format!(
                "fn probe() {{ {locals} let mut dst = 0; {} while false {{ {skipped} }} dst = 7; {} }}",
                "defer {".repeat(depth),
                "}".repeat(depth)
            );
            defer_captures::VISITED_NODES.set(0);
            let program = lir_checked(&source);
            assert_eq!(defer_captures::VISITED_NODES.get(), depth + 2);
            let mut parent = func(&program, "probe");
            for level in 0..depth {
                let site = only_cleanup(parent);
                let id = LirLocalId(if level == 0 { width } else { 0 });
                assert_eq!(site.captures, [id]);
                assert_eq!(parent.locals[id.0 as usize].name, "dst");
                assert_eq!(site.function.locals.len(), 1);
                assert_eq!(site.function.locals[0].name, "dst");
                parent = &site.function;
            }
        }
    }
}

#[test]
fn reachable_cleanup_loops_keep_condition_and_body_captures() {
    for (condition, expected) in [("true", vec![1, 2]), ("flag", vec![0, 1, 2])] {
        let program = lir_checked(&format!(
            "fn probe(flag: bool, src: i64) {{ let mut dst = 0; defer {{ defer {{
                while {condition} {{ dst = src; }}
            }} }} }}"
        ));
        let outer = only_cleanup(func(&program, "probe"));
        assert_eq!(
            outer.captures,
            expected.iter().copied().map(LirLocalId).collect::<Vec<_>>()
        );
        let inner = only_cleanup(&outer.function);
        assert_eq!(
            inner.captures,
            (0..expected.len() as u32)
                .map(LirLocalId)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn nested_cleanup_summaries_share_identifier_bytes() {
    for bytes in [8, 128, 1024] {
        let name = "x".repeat(bytes);
        for depth in [1, 8, 32] {
            for repeated in [false, true] {
                // A local call uses the function identity and a value read uses the HIR spelling.
                let mut block = format!("{name}();");
                for _ in 0..depth {
                    let read = if repeated {
                        format!("let own = {name};")
                    } else {
                        String::new()
                    };
                    block = format!("defer {{ {read} {block} }}");
                }
                let hir = checked_hir(&format!(
                    "fn empty() {{}} fn probe() {{ let padding = 0; let {name} = empty; {block} }}"
                ));
                let probe = hir
                    .functions
                    .iter()
                    .find(|f| f.name.is_free_named("probe"))
                    .unwrap();
                let HirStmt::Defer { body, .. } = probe.body.last().unwrap() else {
                    panic!("expected outer defer")
                };
                let mut sites = HashMap::new();
                let root = defer_captures::analyze(body, &mut sites);
                assert_eq!(sites.len() + 1, depth);
                let mut allocations = HashMap::new();
                for access in sites.values().chain(std::iter::once(&root)) {
                    assert_eq!(access.names.len(), 1);
                    for spelling in &access.names {
                        assert_eq!(&**spelling, name);
                        allocations.insert(spelling.as_ptr(), spelling.len());
                    }
                }
                assert_eq!(allocations.values().sum::<usize>(), bytes);
                let program = lower_source_program(&hir);
                let mut parent = func(&program, "probe");
                for level in 0..depth {
                    let site = only_cleanup(parent);
                    let id = LirLocalId(u32::from(level == 0));
                    assert_eq!(site.captures, [id]);
                    assert_eq!(parent.locals[id.0 as usize].name, name);
                    parent = &site.function;
                }
            }
        }
    }
}

#[test]
fn nested_cleanup_imports_follow_the_lowered_if_branch() {
    for width in [1, 8, 32] {
        let pad: String = (0..width).map(|i| format!("let mut v{i} = {i};")).collect();
        let accesses: String = (0..width)
            .map(|i| format!("v{i} = dst; dst = v{i};"))
            .collect();
        for depth in [1, 8, 32] {
            for condition in ["true", "false", "flag"] {
                let branch = match condition {
                    "true" => format!("if true {{ dst = 7; }} else {{ {accesses} }}"),
                    "false" => format!("if false {{ {accesses} }} dst = 7;"),
                    _ => format!("if flag {{ dst = 7; }} else {{ {accesses} }}"),
                };
                let source = format!(
                    "fn probe(flag: bool) {{ {pad} let mut dst = 0; {} {branch} {} }}",
                    "defer {".repeat(depth),
                    "}".repeat(depth)
                );
                let program = lir_checked(&source);
                let mut parent = func(&program, "probe");
                let all = condition == "flag";
                for level in 0..depth {
                    let body = parent
                        .blocks
                        .iter()
                        .flat_map(|block| &block.instrs)
                        .find_map(|inst| match inst {
                            SourceInst::Defer { body, .. } => Some(body),
                            _ => None,
                        })
                        .expect("nested cleanup");
                    let expected: Vec<_> = if all {
                        (0..width + 2).map(LirLocalId).collect()
                    } else if level == 0 {
                        vec![LirLocalId(width + 1)]
                    } else {
                        vec![LirLocalId(0)]
                    };
                    assert_eq!(body.captures, expected);
                    let (imports, private) = body.function.locals.split_at(expected.len());
                    for (local, id) in imports.iter().zip(&expected) {
                        assert!(local.parameter);
                        assert_eq!(local.name, parent.locals[id.0 as usize].name);
                    }
                    // Only the innermost dynamic branch reserves a condition temporary.
                    if all && level + 1 == depth {
                        assert_eq!(private.len(), 1);
                        assert!(private[0].synthetic && !private[0].parameter);
                        assert_eq!(private[0].ty, Type::Bool);
                    } else {
                        assert!(private.is_empty());
                    }
                    parent = &body.function;
                }
            }
        }
    }
}

#[test]
fn cleanup_conditional_operands_import_only_reachable_bindings() {
    for width in [1u32, 8, 32] {
        let declarations: String = (0..width).map(|i| format!("let v{i} = {i};")).collect();
        let sum = (0..width)
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join(" + ");
        let rhs = format!("({sum} > 0)");
        let dst = width + 2;
        for (expr, reads_flag, reads_live, reads_rhs) in [
            (format!("true ? true : {rhs}"), false, false, false),
            (format!("false ? {rhs} : true"), false, false, false),
            (format!("false && {rhs}"), false, false, false),
            (format!("true || {rhs}"), false, false, false),
            (format!("true && {rhs}"), false, false, true),
            (format!("false || {rhs}"), false, false, true),
            (format!("flag ? live : {rhs}"), true, true, true),
            (format!("flag && {rhs}"), true, false, true),
            (format!("flag || {rhs}"), true, false, true),
            (format!("flag ? (true || {rhs}) : live"), true, true, false),
        ] {
            let parent_ids: Vec<_> = (0..=dst)
                .filter(|id| match *id {
                    0 => reads_flag,
                    1 => reads_live,
                    n if n == dst => true,
                    _ => reads_rhs,
                })
                .map(LirLocalId)
                .collect();
            for depth in [1, 8, 32] {
                let source = format!(
                    "fn probe(flag: bool, live: bool) {{ {declarations} let mut dst = false; {} dst = {expr}; {} }}",
                    "defer {".repeat(depth),
                    "}".repeat(depth)
                );
                let program = lir_checked(&source);
                let mut parent = func(&program, "probe");
                let local_ids: Vec<_> = (0..parent_ids.len() as u32).map(LirLocalId).collect();
                for level in 0..depth {
                    let site = only_cleanup(parent);
                    let expected = if level == 0 { &parent_ids } else { &local_ids };
                    assert_eq!(&site.captures, expected, "{expr} at level {level}");
                    let imports: Vec<_> = site
                        .function
                        .locals
                        .iter()
                        .filter(|l| l.parameter)
                        .collect();
                    assert_eq!(imports.len(), expected.len(), "{expr} at level {level}");
                    for ((import, id), index) in imports.iter().zip(expected).zip(&local_ids) {
                        assert_eq!(import.id, *index);
                        assert_eq!(import.name, parent.locals[id.0 as usize].name);
                    }
                    parent = &site.function;
                }
            }
        }
    }
}

#[test]
fn skipped_cleanup_expression_operands_preserve_syntactic_recovery() {
    for expr in [
        "true ? false : (unused && recover().is_some())",
        "false ? (unused && recover().is_some()) : false",
        "false && (unused || recover().is_some())",
        "true || (unused && recover().is_some())",
    ] {
        let program = lir_checked(&format!(
            "fn probe(unused: bool) {{ defer {{ defer {{ println({expr}); }} }} }}"
        ));
        let mut parent = func(&program, "probe");
        for _ in 0..2 {
            let site = only_cleanup(parent);
            assert!(site.recovery_capable, "{expr}");
            assert!(site.captures.is_empty());
            assert!(site.function.locals.iter().all(|local| !local.parameter));
            parent = &site.function;
        }
    }
}

#[test]
fn skipped_cleanup_branch_preserves_syntactic_recovery() {
    for control in ["if false", "while false"] {
        let program = lir_checked(&format!(
            "fn probe() {{ let unused = 9; defer {{ defer {{
                {control} {{ defer {{ let panic_info = recover(); println(unused); }} }}
            }} }} }}"
        ));
        let mut parent = func(&program, "probe");
        for _ in 0..2 {
            let body = only_cleanup(parent);
            assert!(body.recovery_capable);
            assert!(body.captures.is_empty());
            assert!(body.function.locals.is_empty());
            parent = &body.function;
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
    assert!(names.contains(&"__for0_i$"), "{names:?}");
    assert!(names.contains(&"__for0_end$"), "{names:?}");
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
        Some(SourceInst::Assign { name, .. }) if name == "__for0_i$"
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
    assert!(names.contains(&"__for0_arr$"), "{names:?}");
    assert!(names.contains(&"__for0_i$"), "{names:?}");
    // The length is NOT hoisted into a `let`.
    assert!(!names.contains(&"__for0_len$"), "{names:?}");
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
            SourceInst::Compute { value: LirRvalue::IntrinsicCall { method, .. }, .. } if method.as_str() == "__abi_len")));
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
    assert!(all_lets.iter().any(|n| n == "__for0_i$"), "{all_lets:?}");
    assert!(all_lets.iter().any(|n| n == "__for1_i$"), "{all_lets:?}");
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

const READ_METHOD: &str = "pub fn read(self) -> i64 { return 7; }";

fn method_hierarchy(depth: usize, methods: &str) -> String {
    let mut classes = format!("open class C0 {{ {methods} }} ");
    for i in 1..=depth {
        classes.push_str(&format!("open class C{i} extends C{} {{}} ", i - 1));
    }
    classes
}

#[test]
fn inherited_method_cache_shares_ancestry_across_lowering_paths() {
    use super::declarations::METHOD_ANCESTRY_PROBES;
    for depth in [1, 8, 32] {
        let classes = method_hierarchy(depth, READ_METHOD);
        for count in [1, 8, 32] {
            let scalar = vec!["x.read()"; count].join(",");
            let nested = vec!["(flag ? x : x).read()"; count].join(",");
            let hir = checked_hir(&format!(
                "{classes} fn scalar(x: C{depth}) -> Array<i64> {{ return [{scalar}]; }}
                 fn nested(x: C{depth}, flag: bool) -> Array<i64> {{ return [{nested}]; }}"
            ));
            for (scalar, nested) in [(true, false), (false, true), (true, true)] {
                let mut program = hir.clone();
                program.functions.retain(|f| {
                    (scalar && f.name.is_free_named("scalar"))
                        || (nested && f.name.is_free_named("nested"))
                });
                METHOD_ANCESTRY_PROBES.set(0);
                let lowered = lower_source_program(&program);
                let visits = METHOD_ANCESTRY_PROBES.get();
                assert_eq!(visits, depth + 1);
                assert_eq!(
                    lowered.functions.len(),
                    1 + usize::from(scalar) + usize::from(nested)
                );
                for f in lowered.functions {
                    let calls = f.blocks.iter().flat_map(|b| &b.instrs).filter(|i| matches!(
                        i, SourceInst::Compute { value: LirRvalue::MethodCall { method, .. }, .. } if method == "read"
                    )).count();
                    assert_eq!(
                        calls,
                        if f.name.is_free_named("scalar") || f.name.is_free_named("nested") {
                            count
                        } else {
                            0
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn inherited_method_cache_shares_methodless_routes_across_distinct_names() {
    use super::declarations::{METHOD_ANCESTRY_PROBES, METHOD_TABLE_PROBES};
    for depth in [1, 8, 32] {
        for methods in [1, 8, 32] {
            let names: Vec<_> = (0..methods).map(|i| format!("method{i}")).collect();
            let declarations = names
                .iter()
                .enumerate()
                .map(|(i, name)| format!("pub fn {name}(self) -> i64 {{ return {i}; }}"))
                .collect::<Vec<_>>()
                .join(" ");
            let classes = method_hierarchy(depth, &declarations);
            let calls = |receiver| {
                names
                    .iter()
                    .map(|name| format!("{receiver}.{name}()"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let mut hir = checked_hir(&format!(
                "{classes} fn scalar(x: C{depth}) -> Array<i64> {{ return [{}]; }}
                 fn nested(x: C{depth}, flag: bool) -> Array<i64> {{ return [{}]; }}",
                calls("x"),
                calls("(flag ? x : x)")
            ));
            let base = TypeId::local("C0");
            let leaf = TypeId::local(format!("C{depth}"));
            let view = LoweringDeclarations::new(&hir.resolution);
            METHOD_ANCESTRY_PROBES.set(0);
            METHOD_TABLE_PROBES.set(0);
            let mut peak = 0;
            for _ in 0..2 {
                for name in &names {
                    assert!(std::ptr::eq(
                        view.declared_method(leaf, name).unwrap().signature,
                        &hir.resolution.classes[&base].methods[name]
                    ));
                    peak = peak.max(view.methodless_chain_entries());
                    assert!(
                        view.declared_method(leaf, &format!("missing_{name}"))
                            .is_none()
                    );
                    peak = peak.max(view.methodless_chain_entries());
                }
            }
            let visits = METHOD_ANCESTRY_PROBES.get();
            assert_eq!((peak, visits), (depth + 2, depth + 1));
            let probes = METHOD_TABLE_PROBES.get();
            assert_eq!(probes, 4 * methods);
            println!(
                "methods={methods} depth={depth} peak_entries={peak} ancestry_visits={visits} table_probes={probes}"
            );
            hir.functions
                .retain(|f| f.name.is_free_named("scalar") || f.name.is_free_named("nested"));
            METHOD_ANCESTRY_PROBES.set(0);
            let lowered = lower_source_program(&hir);
            for function in ["scalar", "nested"] {
                let emitted: Vec<_> = func(&lowered, function)
                    .blocks
                    .iter()
                    .flat_map(|b| &b.instrs)
                    .filter_map(|i| match i {
                        SourceInst::Compute {
                            value: LirRvalue::MethodCall { method, .. },
                            ..
                        } => Some(method),
                        _ => None,
                    })
                    .collect();
                assert!(emitted.into_iter().eq(&names));
            }
            assert_eq!(METHOD_ANCESTRY_PROBES.get(), visits);
        }
    }
}

#[test]
fn inherited_method_cache_binds_each_interface_receiver_after_lookup() {
    use super::declarations::METHOD_ANCESTRY_PROBES;
    let hir = checked_hir("interface Pick<T> { fn value(self) -> T; fn receiver(self) -> Self; }");
    let view = LoweringDeclarations::new(&hir.resolution);
    let pick = TypeId::local("Pick");
    let unbound = Type::Named(pick);
    METHOD_ANCESTRY_PROBES.set(0);
    // A failed arity check on the first query must not poison the declaration.
    assert!(method_signature(&view, &unbound, "value").is_none());
    for ty in [Type::Bool, Type::I64, Type::String] {
        let receiver = Type::Generic(pick, vec![ty.clone()]);
        let value = method_signature(&view, &receiver, "value").unwrap();
        assert!(std::ptr::eq(
            value.declaration,
            &hir.resolution.interfaces[&pick].methods["value"]
        ));
        assert_eq!(*value.return_type(), ty);
        assert_eq!(
            *method_signature(&view, &receiver, "receiver")
                .unwrap()
                .return_type(),
            receiver
        );
        assert!(method_signature(&view, &unbound, "value").is_none());
    }
    assert_eq!(METHOD_ANCESTRY_PROBES.get(), 1);
}

#[test]
fn inherited_method_cache_preserves_overrides_and_bounds_cycles() {
    use super::declarations::METHOD_ANCESTRY_PROBES;
    use crate::ir::typed_ast::{HirClassInfo, HirResolution, HirSignature};
    let mut hir = HirResolution::default();
    let [a, b, child, sibling] = ["A", "B", "Child", "Sibling"].map(TypeId::local);
    for (owner, base) in [(a, b), (b, a), (child, a), (sibling, b)] {
        hir.classes.insert(
            owner,
            HirClassInfo {
                base: Some(base),
                ..Default::default()
            },
        );
    }
    for (class, ty) in [(a, Type::I64), (b, Type::Bool)] {
        hir.classes.get_mut(&class).unwrap().methods.insert(
            "which".into(),
            HirSignature {
                params: vec![],
                param_modes: vec![],
                is_static: false,
                is_async: false,
                return_type: ty,
            },
        );
    }
    let view = LoweringDeclarations::new(&hir);
    METHOD_ANCESTRY_PROBES.set(0);
    for _ in 0..8 {
        for (class, owner) in [(child, a), (sibling, b), (a, a), (b, b)] {
            assert!(std::ptr::eq(
                view.declared_method(class, "which").unwrap().signature,
                &hir.classes[&owner].methods["which"]
            ));
            assert!(view.declared_method(class, "missing").is_none());
        }
    }
    // Exercise the complete index on the same cyclic hierarchy.
    for n in 0..16 {
        assert!(view.declared_method(child, &format!("absent{n}")).is_none());
    }
    for (class, owner) in [(child, a), (sibling, b)] {
        assert!(std::ptr::eq(
            view.declared_method(class, "which").unwrap().signature,
            &hir.classes[&owner].methods["which"]
        ));
        assert!(view.declared_method(class, "missing").is_none());
    }
    assert_eq!(METHOD_ANCESTRY_PROBES.get(), 4);
}

// Unknown method names cannot pass the checker. Start with the checked call
// and change its spelling at each dispatcher's validation boundary.
pub(super) fn check_inherited_method_misses(
    mut dispatch: impl for<'r> FnMut(&'r LoweringDeclarations<'r>, &HirExpr) -> bool,
) {
    use super::declarations::METHOD_ANCESTRY_PROBES;
    for depth in [1, 8, 32] {
        let hir = checked_hir(&format!(
            "{} fn probe(x: C{depth}) -> i64 {{ return x.read(); }}",
            method_hierarchy(depth, READ_METHOD)
        ));
        let f = hir
            .functions
            .iter()
            .find(|f| f.name.is_free_named("probe"))
            .unwrap();
        let HirStmt::Return {
            value: Some(hit), ..
        } = &f.body[0]
        else {
            panic!("expected method call")
        };
        let mut miss = hit.clone();
        let HirExprKind::MethodCall { method, .. } = &mut miss.kind else {
            panic!("expected method call")
        };
        *method = "missing".into();
        for count in [1, 8, 32] {
            let declarations = LoweringDeclarations::new(&hir.resolution);
            METHOD_ANCESTRY_PROBES.set(0);
            for _ in 0..count {
                assert!(!dispatch(&declarations, &miss));
            }
            let missing_visits = METHOD_ANCESTRY_PROBES.get();
            assert_eq!(missing_visits, depth + 1);
            assert_eq!(declarations.methodless_chain_entries(), depth + 2);
            for _ in 0..count {
                assert!(dispatch(&declarations, hit));
            }
            assert_eq!(METHOD_ANCESTRY_PROBES.get(), missing_visits);
            assert_eq!(declarations.methodless_chain_entries(), depth + 2);
        }
    }
}

#[test]
fn construction_lookups_share_field_layouts_across_scalar_and_nested_roots() {
    use super::declarations::FIELD_CLASS_VISITS;
    for depth in [1, 8, 32] {
        let mut classes = String::from("open class A0 {} ");
        for i in 1..depth {
            classes.push_str(&format!("open class A{i} extends A{} {{}} ", i - 1));
        }
        classes.push_str(&format!(
            "class Leaf extends A{} {{ pub x: i64; }} ",
            depth - 1
        ));
        for count in [1, 8, 32] {
            let literals = vec!["new Leaf(n)"; count].join(",");
            let constructors = vec!["new Leaf(flag ? n : 0)"; count].join(",");
            let source = format!(
                "{classes}
                fn literals(n: i64) -> Array<Leaf> {{ return [{literals}]; }}
                fn constructed(flag: bool, n: i64) -> Array<Leaf> {{ return [{constructors}]; }}"
            );
            let mut hir = checked_hir(&source);
            // The source language uses new. Exercise the typed object-literal
            // form directly at the scalar entry point.
            let literals = hir
                .functions
                .iter_mut()
                .find(|f| f.name.is_free_named("literals"))
                .unwrap();
            let HirStmt::Return {
                value: Some(root), ..
            } = &mut literals.body[0]
            else {
                panic!("return")
            };
            let HirExprKind::Array { elements } = &mut root.kind else {
                panic!("array of objects")
            };
            for element in elements {
                let HirExprKind::New { class, args } = &mut element.kind else {
                    panic!("memberwise constructor")
                };
                element.kind = HirExprKind::ObjectLiteral {
                    class: *class,
                    fields: vec![("x".into(), args.pop().unwrap())],
                };
            }
            for (scalar, nested) in [(true, false), (false, true), (true, true)] {
                let mut program = hir.clone();
                program.functions.retain(|f| {
                    (scalar && f.name.is_free_named("literals"))
                        || (nested && f.name.is_free_named("constructed"))
                });
                CLASS_FIELD_ENUMERATIONS.set(0);
                FIELD_CLASS_VISITS.set(0);
                let lowered = lower_source_program(&program);
                assert_eq!(CLASS_FIELD_ENUMERATIONS.get(), 1);
                assert_eq!(FIELD_CLASS_VISITS.get(), depth + 1);
                for f in &lowered.functions {
                    let stores = f.blocks.iter().flat_map(|b| &b.instrs).filter(|inst|
                        matches!(inst, SourceInst::Compute { value: LirRvalue::FieldStore { field, .. }, .. } if field == "x")
                    ).count();
                    assert_eq!(stores, count);
                }
                assert_eq!(
                    lowered.functions.len(),
                    usize::from(scalar) + usize::from(nested)
                );
            }
        }
    }
}

#[test]
fn construction_lookups_share_one_variant_index_across_lowering_passes() {
    use super::declarations::{VARIANT_PROBES, VARIANTS_INDEXED};
    for width in [1, 8, 32] {
        let mut variants: Vec<_> = (0..width - 1).map(|i| format!("Unused{i}")).collect();
        variants.push("Last(i64)".into());
        let variants = variants.join(",");
        for count in [1, 8, 32] {
            let scalar = vec!["Wide::Last(n)"; count].join(",");
            let nested = vec!["Wide::Last(flag ? n : 0)"; count].join(",");
            let hir = checked_hir(&format!(
                "enum Wide {{ {variants} }}
                fn scalar(n: i64) -> Array<Wide> {{ return [{scalar}]; }}
                fn nested(n: i64, flag: bool) -> Array<Wide> {{ return [{nested}]; }}"
            ));
            VARIANTS_INDEXED.set(0);
            VARIANT_PROBES.set(0);
            let program = lower_source_program(&hir);
            assert_eq!(VARIANTS_INDEXED.get(), width);
            assert_eq!(VARIANT_PROBES.get(), 2 * count);
            for f in &program.functions {
                let stores = f.blocks.iter().flat_map(|b| &b.instrs).filter(|i|
                    matches!(i, SourceInst::Compute { value: LirRvalue::EnumPayloadStore { variant, .. }, .. } if variant == "Last")
                ).count();
                assert_eq!(stores, count);
            }
        }
    }
}

// Exercise checker-ineligible names directly at the two lowering dispatchers.
pub(super) fn check_empty_and_missing_variants(
    mut dispatch: impl for<'r> FnMut(&'r LoweringDeclarations<'r>, &HirExpr) -> bool,
) {
    use super::declarations::{VARIANT_PROBES, VARIANTS_INDEXED};
    use crate::ir::typed_ast::{HirEnumInfo, HirEnumVariant};
    let class = TypeId::local("Wide");
    for width in [1, 8, 32] {
        let mut hir = crate::ir::typed_ast::HirResolution::default();
        hir.enums.insert(
            class,
            HirEnumInfo {
                type_params: Default::default(),
                variants: (0..width)
                    .map(|i| HirEnumVariant {
                        name: format!("V{i}"),
                        tag: i as i64,
                        payloads: vec![],
                    })
                    .collect(),
            },
        );
        let expr = |name| HirExpr {
            ty: Type::Named(class),
            span: Span::dummy(),
            kind: HirExprKind::StaticCall {
                class,
                method: name,
                args: vec![],
            },
        };
        let present = expr(format!("V{}", width - 1));
        let missing = expr(String::from("Missing"));
        for count in [1, 8, 32] {
            let resolution = LoweringDeclarations::new(&hir);
            VARIANTS_INDEXED.set(0);
            VARIANT_PROBES.set(0);
            for _ in 0..count {
                assert!(!dispatch(&resolution, &missing));
                assert!(dispatch(&resolution, &present));
            }
            assert_eq!(VARIANTS_INDEXED.get(), width);
            assert_eq!(VARIANT_PROBES.get(), 2 * count);
        }
    }
}
