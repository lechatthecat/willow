use super::*;

#[test]
fn effect_solution_relocation_preserves_lock_target_and_invalidates_changed_evidence() {
    let owner = FunctionId::free("work");
    let old = Span::new(10, 20, 1, 11);
    let new = Span::new(30, 40, 2, 11);
    let target = Span::new(15, 16, 1, 16);
    let new_target = Span::new(35, 36, 2, 16);
    let witness = EffectWitness::Lock(LockEffectWitness {
        owner,
        cause: LockEffectCause {
            is_conservative: false,
            span: old,
            operation: "lock".into(),
            kind: LockEffectKind::Suspend,
            lock: Some(Box::new(LockTarget {
                mode: "lock".into(),
                target,
                r#type: Some(Type::I64),
            })),
        },
    });
    let solution = EffectProblem::new()
        .seed(owner, RuntimeEffects::MAY_SUSPEND, Some(witness))
        .solve_incremental(&CallGraph::default(), None);
    let queries = EffectQueries::default();
    queries
        .solutions
        .borrow_mut()
        .insert(UnitId::ENTRY, Arc::new(solution));
    let mut correspondence = super::super::syntax::Correspondence::default();
    correspondence
        .spans
        .extend([(old, new), (target, new_target)]);
    queries.relocate_solutions(&correspondence);
    let solutions = queries.solutions.borrow();
    let fact = solutions[&UnitId::ENTRY].facts.get(&owner).unwrap();
    let EffectWitness::Lock(lock) = fact.witness(RuntimeEffects::MAY_SUSPEND).unwrap() else {
        panic!("lock evidence");
    };
    assert_eq!(lock.cause.span, new);
    assert_eq!(lock.cause.lock.as_ref().unwrap().target, new_target);
    for change in ["type", "mode", "target", "conservative", "removed"] {
        let mut changed = lock.clone();
        match change {
            "type" => changed.cause.lock.as_mut().unwrap().r#type = Some(Type::Bool),
            "mode" => changed.cause.lock.as_mut().unwrap().mode = "lock read".into(),
            "target" => changed.cause.lock.as_mut().unwrap().target = target,
            "conservative" => changed.cause.is_conservative = true,
            "removed" => changed.cause.lock = None,
            _ => unreachable!(),
        }
        let changed = EffectProblem::new()
            .seed(
                owner,
                RuntimeEffects::MAY_SUSPEND,
                Some(EffectWitness::Lock(changed)),
            )
            .solve_incremental_by(
                &CallGraph::default(),
                Some(&solutions[&UnitId::ENTRY]),
                same_seed,
            );
        assert_eq!(changed.recomputed, 1, "{change}");
    }
}

#[test]
fn memberwise_constructor_facts_scale_without_local_uses() {
    use crate::query_stats::{Counter, Session, count};
    // Empty/wide classes, inheritance chains, and repeated references all
    // publish one summary per declaration, independent of use frequency.
    for shape in ["empty", "wide", "chain", "repeated", "enum"] {
        for size in [1, 8, 64, 256] {
            let mut source = String::new();
            for i in 0..size {
                let base = if shape == "chain" && i > 0 {
                    format!(" extends C{}", i - 1)
                } else {
                    String::new()
                };
                let fields = if shape == "wide" {
                    (0..32)
                        .map(|j| format!("pub f{j}: i64;"))
                        .collect::<String>()
                } else {
                    String::new()
                };
                if shape == "enum" {
                    source.push_str(&format!("enum C{i} {{ Value(i64) }}\n"));
                } else {
                    source.push_str(&format!("open class C{i}{base} {{ {fields} }}\n"));
                }
            }
            let program = program(&source);
            let mut graph = CallGraph::default();
            if shape == "repeated" {
                for i in 0..size {
                    let mut sites = crate::semantic::call_graph::CallSites::default();
                    sites
                        .targets
                        .insert(FunctionId::method(TypeId::local("C0"), "init"));
                    graph.merge(FunctionId::free(format!("caller{i}")), sites);
                }
            }
            let _session = Session::start(true, false);
            let effects = solve_unit(
                &program,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &HashMap::new(),
                |_| panic!("local constructor treated as external"),
            );
            for i in 0..size {
                let id = FunctionId::method(
                    TypeId::local(format!("C{i}")),
                    if shape == "enum" { "Value" } else { "init" },
                );
                assert_eq!(
                    effects.facts.get(&id).unwrap().effects(),
                    RuntimeEffects::NONE
                );
            }
            assert_eq!(count(Counter::EffectInventory), size);
            assert_eq!(
                effects.facts.iter().count(),
                size * if shape == "repeated" { 2 } else { 1 }
            );
            assert_eq!(count(Counter::EffectSolve), 1);
            eprintln!(
                "memberwise shape={shape} classes={size} inventory={} facts={}",
                count(Counter::EffectInventory),
                effects.facts.iter().count()
            );
        }
    }
}

#[test]
fn complete_dispatch_effects_scale_and_incomplete_slots_fail_closed() {
    use crate::query_stats::{Counter, Session, count};
    use crate::semantic::call_graph::CallSites;
    let program = program("fn leaf() {} fn caller() {}");
    let leaf = FunctionId::free("leaf");
    let caller = FunctionId::free("caller");
    for chain in [false, true] {
        for size in [1, 8, 64, 256] {
            let mut graph = CallGraph::default();
            let mut callables = HashMap::new();
            graph.merge(leaf, CallSites::default());
            let mut targets = Vec::new();
            for i in 0..size {
                let id = FunctionId::method(TypeId::local(format!("I{i}")), "run");
                let target = if chain && i > 0 { targets[i - 1] } else { leaf };
                graph.merge(id, target_sites([target].into()));
                callables.insert(id, false);
                targets.push(id);
            }
            graph.merge(caller, target_sites(targets.into_iter().collect()));
            let _session = Session::start(true, false);
            let effects = solve_unit(
                &program,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &callables,
                &HashMap::new(),
                |_| panic!("complete local dispatch classified as external"),
            );
            assert!(effects.facts.get(&caller).unwrap().is_empty());
            assert_eq!(effects.facts.iter().count(), size + 2);
            assert_eq!(count(Counter::EffectEdges), 3 * size);
            println!(
                "dispatch chain={chain} unions={size} facts={} classified_edges={}",
                size + 2,
                count(Counter::EffectEdges)
            );
        }
    }
    for (registered, nonempty, unknown) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let target = FunctionId::method(TypeId::local("Missing"), "run");
        let mut graph = CallGraph::default();
        graph.merge(leaf, CallSites::default());
        graph.merge(caller, target_sites([target].into()));
        graph.merge(
            target,
            CallSites {
                targets: if nonempty {
                    [leaf].into()
                } else {
                    Default::default()
                },
                has_unknown: unknown,
                ..Default::default()
            },
        );
        let callables = if registered {
            [(target, false)].into()
        } else {
            HashMap::new()
        };
        let effects = solve_unit(
            &program,
            &graph,
            &HashMap::<ExprId, Type>::new(),
            None,
            &callables,
            &HashMap::new(),
            |_| PANIC,
        );
        assert!(effects.facts.get(&caller).unwrap().contains(PANIC));
    }
}

#[test]
fn imported_generic_targets_resolve_module_owners_without_losing_classes() {
    for path in [
        "boxes",
        "$local::nested::boxes",
        "$pkg0123456789abcdef0123456789abcdef::boxes",
    ] {
        let mut queries = EffectQueries::default();
        let provider = crate::module::ModuleId(1);
        queries.names.insert(path.into(), provider);
        let name = "id$mono$abc";
        let free = FunctionId::method(TypeId::from_source_name(path), name);
        assert_eq!(
            queries.external_target(UnitId::ENTRY, &free, &HashMap::new()),
            Some((provider, FunctionId::free(name)))
        );
        let method = FunctionId::method(TypeId::from_source_name(&format!("{path}::Box")), "get");
        assert_eq!(
            queries.external_target(UnitId::ENTRY, &method, &HashMap::new()),
            Some((provider, FunctionId::method(TypeId::local("Box"), "get")))
        );
    }
}

#[test]
fn deep_read_only_scans_use_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let tokens = crate::lexer::Lexer::new("async fn f() { await sleep(1); }")
                .tokenize()
                .unwrap();
            let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty());
            let Item::Function(function) = &mut program.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(expr) = &mut function.body.stmts[0] else {
                unreachable!()
            };
            let span = crate::diagnostics::Span::new(0, 0, 1, 1);
            let mut expr =
                std::mem::replace(&mut expr.expr, crate::parser::ownership::placeholder_expr());
            for _ in 0..50_000 {
                expr =
                    Expr::TryPropagate(Box::new(expr), span, crate::parser::ast::ExprId::fresh());
            }
            function
                .body
                .stmts
                .push(Stmt::Expr(crate::parser::ast::ExprStmt { expr, span }));
            let mut hazards = HazardVisitor {
                panics: false,
                checked_panics: false,
                checked_span: None,
                panic_span: None,
                expr_types: &HashMap::<ExprId, Type<TypeId>>::new(),
                overflow_checks: true.into(),
            };
            hazards.visit_block(&function.body);
            assert!(hazards.panics);
            drop(program);
        })
        .unwrap()
        .join()
        .unwrap();
}

fn program(source: &str) -> Program {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    program
}

#[test]
fn overflow_override_panic_effects_distinguish_arithmetic_and_shifts() {
    for (body, arithmetic) in [
        ("return a + 1;", true),
        ("return a - 1;", true),
        ("return a * 2;", true),
        ("return -a;", true),
        ("return a ** 3;", true),
        ("return a << a;", false),
        ("return a >> 64;", false),
    ] {
        let program = program(&format!("fn f(a: i64) -> i64 {{ {body} }}"));
        let Item::Function(function) = &program.items[0] else {
            unreachable!()
        };
        let mut types = HashMap::new();
        for event in AstWalk::new(AstEvent::Block(&function.body)) {
            if let AstEvent::Expr(expression) = event {
                types.insert(expression.id(), Type::<String>::I64);
            }
        }
        for i64_arithmetic in [false, true] {
            for other in [false, true] {
                let mut hazards = HazardVisitor {
                    panics: false,
                    checked_panics: false,
                    checked_span: None,
                    panic_span: None,
                    expr_types: &types,
                    overflow_checks: OverflowChecks {
                        i64_arithmetic,
                        other,
                    },
                };
                hazards.visit_block(&function.body);
                let enabled = if arithmetic { i64_arithmetic } else { other };
                assert_eq!(hazards.panics, enabled, "{body} {i64_arithmetic} {other}");
                assert_eq!(hazards.checked_panics, !enabled);
            }
        }
    }
}

#[test]
fn integer_arithmetic_may_panic_only_with_overflow_checks() {
    // Without expression types `+` stays a possible string concatenation,
    // which may panic regardless of the mode, so it is not listed here.
    for (body, checked_panics) in [
        ("return a - 1;", true),
        ("return a * 2;", true),
        ("return -a;", true),
        ("return -5;", false),
        ("if a < 1 { return 0; } return a;", false),
        // Shifts check a runtime amount; an in-range literal cannot fault.
        ("return 1 << a;", true),
        ("return a >> a;", true),
        ("return a << 64;", true),
        ("return a >> -1;", true),
        ("return a << 63;", false),
        ("return a >> 0;", false),
        ("return a & 3 | a ^ 5;", false),
        ("return !a;", false),
        // A literal exponent unrolls into checked multiplications; `0` and
        // `1` multiply nothing.
        ("return a ** 3;", true),
        ("return a ** 1;", false),
        ("return a ** 0;", false),
    ] {
        let program = program(&format!("fn f(a: i64) -> i64 {{ {body} }}"));
        let Item::Function(function) = &program.items[0] else {
            unreachable!()
        };
        for overflow_checks in [true, false] {
            let mut hazards = HazardVisitor {
                panics: false,
                checked_panics: false,
                checked_span: None,
                panic_span: None,
                expr_types: &HashMap::<ExprId, Type<TypeId>>::new(),
                overflow_checks: overflow_checks.into(),
            };
            hazards.visit_block(&function.body);
            assert_eq!(
                hazards.panics,
                checked_panics && overflow_checks,
                "{body} overflow_checks={overflow_checks}"
            );
            // Release keeps the same answer in the profile-independent bit.
            assert_eq!(
                hazards.checked_panics,
                checked_panics && !overflow_checks,
                "{body} overflow_checks={overflow_checks}"
            );
        }
    }
}

#[test]
fn runtime_integer_exponent_keeps_its_sign_guard_in_every_mode() {
    for body in ["return a ** a;", "return a ** (1 + 2);", "return 2 ** a;"] {
        let program = program(&format!("fn f(a: i64) -> i64 {{ {body} }}"));
        let Item::Function(function) = &program.items[0] else {
            unreachable!()
        };
        for overflow_checks in [true, false] {
            let mut hazards = HazardVisitor {
                panics: false,
                checked_panics: false,
                checked_span: None,
                panic_span: None,
                expr_types: &HashMap::<ExprId, Type<TypeId>>::new(),
                overflow_checks: overflow_checks.into(),
            };
            hazards.visit_block(&function.body);
            assert!(hazards.panics, "{body} overflow_checks={overflow_checks}");
        }
    }
}

#[test]
fn float_exponent_has_no_guard_but_untyped_power_stays_conservative() {
    let program = program("fn f(a: f64) -> f64 { return a ** 0.5; }");
    let Item::Function(function) = &program.items[0] else {
        unreachable!()
    };
    let Stmt::Return(ret) = &function.body.stmts[0] else {
        unreachable!()
    };
    let power = ret.value.as_ref().unwrap().id();
    for (types, panics) in [
        (HashMap::new(), true),
        (HashMap::from([(power, Type::<TypeId>::F64)]), false),
        (HashMap::from([(power, Type::<TypeId>::I64)]), true),
    ] {
        let mut hazards = HazardVisitor {
            panics: false,
            checked_panics: false,
            checked_span: None,
            panic_span: None,
            expr_types: &types,
            overflow_checks: true.into(),
        };
        hazards.visit_block(&function.body);
        assert_eq!(hazards.panics, panics, "{types:?}");
        assert!(!hazards.checked_panics);
    }
}

#[test]
fn lock_acquisition_seeds_suspend_and_preserves_its_witness() {
    use crate::semantic::call_graph::CallSites;
    for mode in ["", "read ", "write "] {
        for prefix in ["", "await sleep(0); "] {
            let source = format!(
                "async fn acquire() {{ {prefix}lock {mode}cell as value {{ }} }} fn relay() {{ }}"
            );
            let program = program(&source);
            let acquire = FunctionId::free("acquire");
            let relay = FunctionId::free("relay");
            let mut graph = CallGraph::default();
            graph.merge(
                relay,
                CallSites {
                    unsupported_initialization: Default::default(),
                    call_spans: Default::default(),
                    indirect_spans: Default::default(),
                    unknown_span: None,
                    virtual_spans: Default::default(),
                    virtual_calls: Default::default(),
                    targets: [acquire].into(),
                    has_unknown: false,
                },
            );
            let direct = HashMap::from([(
                acquire,
                DirectLockEffects {
                    causes: vec![LockEffectCause {
                        is_conservative: false,
                        span: Span::dummy(),
                        operation: "earlier wait".into(),
                        kind: LockEffectKind::SuspendOrBlock,
                        lock: None,
                    }],
                },
            )]);
            let lock_only = solve_unit(
                &program,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &HashMap::new(),
                |_| PANIC,
            );
            for id in [acquire, relay] {
                assert!(lock_only.facts.intersects(&id, RuntimeEffects::MAY_SUSPEND));
                assert!(!lock_only.facts.intersects(&id, RuntimeEffects::MAY_BLOCK));
            }
            let effects = solve_unit(
                &program,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &direct,
                |_| PANIC,
            );
            for id in [acquire, relay] {
                let summary = effects.facts.get(&id).unwrap();
                assert!(summary.contains(LOCK_EFFECT_WAIT));
                let cause = summary
                    .witness(RuntimeEffects::MAY_SUSPEND)
                    .unwrap()
                    .lock()
                    .unwrap();
                assert_eq!(cause.operation, "lock");
                assert_eq!(cause.span.start, source.find("lock ").unwrap());
                assert_eq!(
                    summary
                        .witness(RuntimeEffects::MAY_BLOCK)
                        .unwrap()
                        .lock()
                        .unwrap()
                        .operation,
                    "earlier wait"
                );
            }
        }
    }
}

#[test]
fn lock_scan_isolates_lambda_and_roundtrips_cached_evidence() {
    let program = program("async fn outer() { let f = || { lock cell as value {} }; }");
    let Item::Function(function) = &program.items[0] else {
        panic!("function")
    };
    let direct = scan_direct_effects(
        FunctionId::free("outer"),
        function.body.id,
        &function.body,
        &HashMap::<ExprId, Type>::new(),
        None,
        true.into(),
    );
    assert_eq!(direct.len(), 2);
    assert!(direct[0].lock_span.is_none());
    assert!(direct[1].lock_span.is_some());
    let encoded = serde_json::to_vec(&direct).unwrap();
    let restored: Vec<DirectBodyEffects> = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(restored[1].lock_span, direct[1].lock_span);
}

#[test]
fn cached_lock_evidence_relocates_after_coordinate_only_edit() {
    let before = program("async fn acquire() { lock cell as value {} }");
    let mut after =
        program("\n// shifted declaration\nasync fn acquire() { lock cell as value {} }");
    let Item::Function(old) = &before.items[0] else {
        panic!("function");
    };
    let cached = scan_direct_effects(
        FunctionId::free("acquire"),
        old.body.id,
        &old.body,
        &HashMap::<ExprId, Type>::new(),
        None,
        true.into(),
    );
    let mut correspondence = super::super::syntax::Correspondence::default();
    correspondence.reconcile(&before, &mut after).unwrap();
    let Item::Function(new) = &after.items[0] else {
        panic!("function");
    };
    assert_eq!(new.body.id, old.body.id, "unchanged body is reused");
    // BodyQueries::derived uses this same remapping for cached direct facts.
    let relocated: Vec<DirectBodyEffects> = correspondence.remap(&cached).unwrap();
    let Stmt::Lock(lock) = &new.body.stmts[0] else {
        panic!("lock");
    };
    assert_eq!(relocated[0].lock_span, Some(lock.span));
    assert_ne!(relocated[0].lock_span, cached[0].lock_span);
    assert_eq!(relocated[0].lock_span.unwrap().line, 3);
    // The operand range moves with its statement.
    let target = relocated[0].lock_target.as_ref().unwrap();
    assert_eq!(target.target, lock.target_span);
    assert_eq!(target.target.line, 3);
}

#[test]
fn lock_scan_records_operand_mode_range_and_checked_type() {
    for (mode, keyword, ty) in [
        ("", "lock", "Mutex"),
        ("read ", "lock read", "RwLock"),
        ("write ", "lock write", "RwLock"),
    ] {
        let source = format!(
            "async fn f(h: Holder) {{ lock {mode}h\n  .cache as value {{}} lock {mode}other as v {{}} }}"
        );
        let program = program(&source);
        let Item::Function(function) = &program.items[0] else {
            panic!("function");
        };
        let Stmt::Lock(first) = &function.body.stmts[0] else {
            panic!("lock");
        };
        let checked = Type::Generic(
            ty.into(),
            vec![Type::Generic("Map".into(), vec![Type::I64, Type::I64])],
        );
        let types = HashMap::from([(first.target.id(), checked.clone())]);
        let direct = scan_direct_effects(
            FunctionId::free("f"),
            function.body.id,
            &function.body,
            &types,
            None,
            true.into(),
        );
        // The first acquisition is the witness; its operand is the whole
        // member chain, not only the field token `target.span()` names.
        let target = direct[0].lock_target.as_ref().unwrap();
        assert_eq!(target.mode, keyword);
        assert_eq!(
            &source[target.target.start..target.target.end],
            "h\n  .cache"
        );
        assert_eq!(target.r#type.as_ref(), Some(&checked));
        // Serialized cause carries the operand; caches round-trip it.
        let encoded = serde_json::to_value(&direct).unwrap();
        assert_eq!(encoded[0]["lock_target"]["mode"], keyword);
        let restored: Vec<DirectBodyEffects> = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            restored[0].lock_target.as_ref().unwrap().target,
            target.target
        );
    }
    // An untyped operand still names its range; the type is omitted.
    let program = program("async fn f() { lock m as v {} }");
    let Item::Function(function) = &program.items[0] else {
        panic!("function");
    };
    let direct = scan_direct_effects(
        FunctionId::free("f"),
        function.body.id,
        &function.body,
        &HashMap::<ExprId, Type>::new(),
        None,
        true.into(),
    );
    let target = direct[0].lock_target.as_ref().unwrap();
    assert!(target.r#type.is_none());
    let cause = LockEffectCause {
        is_conservative: false,
        span: target.target,
        operation: "lock".into(),
        kind: LockEffectKind::Suspend,
        lock: Some(Box::new(target.clone())),
    };
    let wire = serde_json::to_value(&cause).unwrap();
    assert!(wire["lock"].get("type").is_none(), "{wire}");
    // Non-lock waits serialize without a `lock` member.
    let wait = LockEffectCause {
        lock: None,
        ..cause
    };
    assert!(serde_json::to_value(&wait).unwrap().get("lock").is_none());
}

#[test]
fn buildgraph_cached_loop_evidence_relocates() {
    let before = program("fn work() { while true {} }");
    let mut after = program("\n// move loop\nfn work() { while true {} }");
    let Item::Function(old) = &before.items[0] else {
        panic!("function");
    };
    let cached = scan_direct_effects(
        FunctionId::free("work"),
        old.body.id,
        &old.body,
        &HashMap::<ExprId, Type>::new(),
        None,
        true.into(),
    );
    let mut correspondence = super::super::syntax::Correspondence::default();
    correspondence.reconcile(&before, &mut after).unwrap();
    let Item::Function(new) = &after.items[0] else {
        panic!("function");
    };
    let relocated: Vec<DirectBodyEffects> = correspondence.remap(&cached).unwrap();
    assert_eq!(relocated[0].loop_span, Some(new.body.stmts[0].span()));
    assert_ne!(relocated[0].loop_span, cached[0].loop_span);
}

#[test]
fn combined_masks_keep_lock_helper_and_panic_contracts_distinct() {
    use crate::semantic::call_graph::CallSites;
    let program = program(
        "fn leaf() { while true {} let n = 1 / 0; } async fn task() {} fn async_only() {} fn constructor_only() {} fn lambda_only() {} fn union_only() {} fn helper_only() {} class C { init(self) {} }",
    );
    let leaf = FunctionId::free("leaf");
    let task = FunctionId::free("task");
    let constructor = FunctionId::method(TypeId::local("C"), "init");
    let lambda = FunctionId::lambda(BodyId::fresh());
    let dispatch = FunctionId::method(TypeId::local("I"), "run");
    let mut graph = CallGraph::default();
    graph.merge(leaf, CallSites::default());
    for (caller, target) in [
        (task, leaf),
        (constructor, leaf),
        (lambda, leaf),
        (dispatch, leaf),
        (FunctionId::free("async_only"), task),
        (FunctionId::free("constructor_only"), constructor),
        (FunctionId::free("lambda_only"), lambda),
        (FunctionId::free("union_only"), dispatch),
        (FunctionId::free("helper_only"), leaf),
    ] {
        graph.merge(
            caller,
            CallSites {
                unsupported_initialization: Default::default(),
                call_spans: Default::default(),
                indirect_spans: Default::default(),
                unknown_span: None,
                virtual_spans: Default::default(),
                virtual_calls: Default::default(),
                targets: [target].into(),
                has_unknown: false,
            },
        );
    }
    let direct = HashMap::from([(
        leaf,
        DirectLockEffects {
            causes: vec![LockEffectCause {
                is_conservative: false,
                span: Span::dummy(),
                operation: "wait".into(),
                kind: LockEffectKind::Block,
                lock: None,
            }],
        },
    )]);
    let effects = solve_unit(
        &program,
        &graph,
        &HashMap::<ExprId, Type>::new(),
        None,
        &HashMap::from([(task, true)]),
        &direct,
        |_| PANIC,
    );
    for name in [
        "async_only",
        "constructor_only",
        "lambda_only",
        "union_only",
        "helper_only",
    ] {
        let id = FunctionId::free(name);
        assert!(effects.facts.intersects(&id, PANIC), "{name}");
        assert_eq!(
            effects.facts.intersects(&id, LOCK_EFFECT_WAIT),
            name != "async_only",
            "{name}"
        );
        assert_eq!(
            effects.helpers.contains_key(&id),
            name == "helper_only",
            "{name}"
        );
    }
    assert!(effects.facts.intersects(&task, LOCK_EFFECT_WAIT));
    assert!(
        effects
            .facts
            .get(&FunctionId::free("constructor_only"))
            .unwrap()
            .witness(LOCK_EFFECT_WAIT)
            .unwrap()
            .lock()
            .is_some()
    );
}

#[test]
fn inventory_counts_scale_with_syntax_and_edges_and_classify_shared_targets_once() {
    use crate::query_stats::{Counter, Session, count};
    for shape in ["chain", "fanout", "repeated", "cycle", "locks"] {
        let mut previous = None;
        for size in [16usize, 64, 256, 1024] {
            let mut source = String::new();
            for i in 0..size {
                let target = match shape {
                    "chain" if i + 1 < size => format!("node_{}", i + 1),
                    "cycle" => format!("node_{}", (i + 1) % size),
                    _ => "external".to_string(),
                };
                let lock = if shape == "locks" {
                    "lock cell as value {}"
                } else {
                    ""
                };
                source.push_str(&format!("fn node_{i}() {{ {target}(); {lock} }}\n"));
            }
            if shape == "fanout" || shape == "repeated" {
                source.push_str("fn root() { ");
                for i in 0..size {
                    source.push_str(&format!(
                        "node_{}();",
                        if shape == "repeated" { 0 } else { i }
                    ));
                }
                source.push('}');
            }
            let program = program(&source);
            let graph = crate::semantic::TypeChecker::resolved_effect_graph(&program);
            let calls = std::cell::Cell::new(0);
            let _session = Session::start(true, false);
            solve_unit(
                &program,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &HashMap::new(),
                |_| {
                    calls.set(calls.get() + 1);
                    PANIC
                },
            );
            assert_eq!(count(Counter::EffectSolve), 1);
            assert_eq!(count(Counter::EffectIoSolve), 0);
            assert_eq!(calls.get(), usize::from(shape != "cycle"));
            let work = count(Counter::EffectInventory) + count(Counter::EffectEdges);
            if let Some((previous_size, previous_work)) = previous {
                // Root scaffolding is constant; all variable work is linear.
                assert!(work <= previous_work * (size / previous_size));
            }
            previous = Some((size, work));
            eprintln!(
                "effect-inventory shape={shape} size={size} ast={} edges={} solves=1",
                count(Counter::EffectInventory),
                count(Counter::EffectEdges)
            );
        }
    }
}

#[test]
fn tracked_capabilities_backdate_and_gate_real_solver() {
    use super::super::incremental::SyntaxQueries;
    use std::{cell::RefCell, path::Path, rc::Rc};
    let path = Path::new("tracked_effect.wi");
    let root = BodyId::fresh();
    let id = FunctionId::free("get");
    let consumer = crate::module::ModuleId(123);
    let consumer_id = FunctionId::free("consumer");
    let mut accepted = SyntaxQueries::default();
    let mut previous = None;
    let solves = std::cell::Cell::new(0);
    for (step, source) in [
        "fn get() -> i64 { return 1; }",
        "fn get() -> i64 { return 2; }",
        "fn get() -> i64 { println(2); return 2; }",
        "fn get() -> i64 { println(2); return 2; }",
    ]
    .into_iter()
    .enumerate()
    {
        let tracking = Rc::new(RefCell::new(accepted.candidate().unwrap()));
        let (mut source, _, _) = tracking
            .borrow_mut()
            .parse(path, crate::diagnostics::FileId::ENTRY, source)
            .unwrap();
        let Item::Function(function) = &mut source.items[0] else {
            panic!()
        };
        function.body.id = root;
        if !tracking
            .borrow_mut()
            .validate_body(UnitId::ENTRY, root, path, "Function:get", vec![])
            .unwrap()
        {
            tracking
                .borrow_mut()
                .publish_body(UnitId::ENTRY, root, serde_json::Value::Null, vec![], vec![])
                .unwrap();
        }
        let queries = EffectQueries::default();
        *queries.tracking.borrow_mut() = Rc::downgrade(&tracking);
        queries.roots.borrow_mut().insert(UnitId::ENTRY, vec![root]);
        if let Some(previous) = &previous {
            queries.reuse_from(previous);
        }
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
        queries
            .complete(UnitId::ENTRY, || {
                solves.set(solves.get() + 1);
                solve_unit(
                    &source,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| PANIC,
                )
            })
            .unwrap();
        let capabilities = tracking
            .borrow()
            .effect_capabilities(UnitId::ENTRY, id)
            .unwrap();
        assert_eq!(capabilities.may_io, step >= 2);
        let consumer_green = tracking
            .borrow_mut()
            .validate_effects(consumer, vec![])
            .unwrap();
        assert_eq!(consumer_green, step == 1 || step == 3);
        if !consumer_green {
            tracking
                .borrow_mut()
                .publish_effects(
                    consumer,
                    vec![],
                    vec![(UnitId::ENTRY, id)],
                    HashMap::from([(
                        consumer_id,
                        CapturedEffect {
                            capabilities,
                            evidence: EffectEvidence::new(EffectSummary::default()),
                            helper: None,
                            loop_bodies: Vec::new(),
                        },
                    )]),
                    vec![],
                    serde_json::Value::Null,
                )
                .unwrap();
        }
        assert_eq!(solves.get(), (step + 1).min(3));
        previous = Some(queries);
        accepted = Rc::try_unwrap(tracking).unwrap().into_inner();
    }
}

#[test]
fn tracked_lock_evidence_equality_includes_location_and_cause() {
    let summary = |offset| {
        EffectSummary::new(
            LOCK_EFFECT_WAIT,
            Some(EffectWitness::Lock(LockEffectWitness {
                owner: FunctionId::free("wait"),
                cause: LockEffectCause {
                    is_conservative: false,
                    span: Span::new(offset, offset + 2, 1, offset + 1),
                    operation: "sleep".into(),
                    kind: LockEffectKind::Suspend,
                    lock: None,
                },
            })),
        )
    };
    // In-solve witness ordering intentionally identifies a cause by owner.
    assert_eq!(summary(1), summary(7));
    assert!(!same_seed(&summary(1), &summary(7)));
    assert_ne!(
        EffectEvidence::new(summary(1)),
        EffectEvidence::new(summary(7))
    );
}

/// The compiler-internal checked-panic bit owns its own witness slot past
/// the runtime bits; seed equality and size accounting must include it.
#[test]
fn checked_panic_witness_participates_in_seed_equality() {
    use super::super::retained::Retained;
    let checked = RuntimeEffects::MAY_CHECKED_PANIC;
    let summary = |offset| {
        EffectSummary::new(
            checked,
            Some(EffectWitness::Panic {
                owner: FunctionId::free("add"),
                source: (0, offset, offset + 3),
            }),
        )
    };
    assert_eq!(
        summary(4).witness(checked),
        Some(&EffectWitness::Panic {
            owner: FunctionId::free("add"),
            source: (0, 4, 7),
        })
    );
    assert!(same_seed(&summary(4), &summary(4)));
    assert!(!same_seed(&summary(4), &summary(9)));
    assert_ne!(
        EffectEvidence::new(summary(4)),
        EffectEvidence::new(summary(9))
    );
    let runtime_slots = RuntimeEffects::BIT_COUNT as usize;
    assert_eq!(
        EffectEvidence::new(summary(4)).heap_bytes(),
        (runtime_slots + 1) * (std::mem::size_of::<EffectWitness>() + 64)
    );
}

#[test]
fn callable_capabilities_separate_io_and_evidence() {
    fn evaluate(source: &str) -> EffectQueries {
        let queries = EffectQueries::default();
        let source = program(source);
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
        queries
            .complete(UnitId::ENTRY, || {
                solve_unit(
                    &source,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| PANIC,
                )
            })
            .unwrap();
        queries
    }
    let first = evaluate("fn get() -> i64 { return 1; } fn caller() -> i64 { return get(); }");
    let second = evaluate("fn get() -> i64 { return 2; } fn caller() -> i64 { return get(); }");
    let io =
        evaluate("fn get() -> i64 { println(2); return 2; } fn caller() -> i64 { return get(); }");
    for name in ["get", "caller"] {
        let id = FunctionId::free(name);
        assert_eq!(
            first.effect_capabilities(UnitId::ENTRY, id),
            second.effect_capabilities(UnitId::ENTRY, id)
        );
        assert!(!first.effect_capabilities(UnitId::ENTRY, id).unwrap().may_io);
        assert!(io.effect_capabilities(UnitId::ENTRY, id).unwrap().may_io);
    }
    let first = evaluate("fn danger(n: i64) -> i64 { return 1 / n; }");
    let shifted = evaluate("  fn danger(n: i64) -> i64 { return 1 / n; }");
    let id = FunctionId::free("danger");
    assert_eq!(
        first.effect_capabilities(UnitId::ENTRY, id),
        shifted.effect_capabilities(UnitId::ENTRY, id)
    );
    assert_ne!(
        first.effect_evidence(UnitId::ENTRY, id),
        shifted.effect_evidence(UnitId::ENTRY, id)
    );
}

#[test]
fn imported_interface_missing_facts_keep_waits_without_fabricated_witnesses() {
    for available in [false, true] {
        let mut queries = EffectQueries::default();
        let dependency = crate::module::ModuleId(7);
        queries.names.insert("dependency".into(), dependency);
        let worker = program(
            "pub interface Player { fn choose(self); } pub class RandomPlayer implements Player { pub fn choose(self) {} }",
        );
        let queries = std::rc::Rc::new(queries);
        if available {
            let mut provider = crate::semantic::TypeChecker::new();
            provider.set_effect_queries(queries.clone(), dependency);
            provider.check_program(&worker);
            assert!(provider.errors.is_empty(), "{:?}", provider.errors);
        }
        let mut checker = crate::semantic::TypeChecker::new();
        checker.register_module("alias", "dependency", "dependency", &worker);
        checker.set_effect_queries(queries.clone(), UnitId::ENTRY);
        checker.check_program(&program("import dependency as alias; fn invoke(p: alias::Player) { p.choose(); } async fn main(p: alias::Player) { let m = Mutex::new(0); lock m as n { invoke(p); } }"));
        assert_eq!(
            checker.errors.len(),
            usize::from(!available),
            "{:?}",
            checker.errors
        );
        if !available {
            assert_eq!(checker.errors[0].code, crate::diagnostics::ErrorCode::E2604);
        }
        let fact = queries
            .effect_evidence(UnitId::ENTRY, FunctionId::free("invoke"))
            .unwrap();
        assert_eq!(
            fact.effects().intersection(LOCK_EFFECT_WAIT),
            if available {
                RuntimeEffects::NONE
            } else {
                LOCK_EFFECT_WAIT
            }
        );
        assert!(fact.witness(LOCK_EFFECT_WAIT).is_none(), "{fact:?}");
    }
}

#[test]
fn effect_contract_imported_proofs_require_current_verification() {
    use crate::diagnostics::ErrorCode;
    for mode in ["missing", "current", "stale", "waiting", "callback", "ffi"] {
        let dependency = crate::module::ModuleId(7);
        let mut worker = program(match mode {
            "waiting" => "pub fn run(c: BlockingCell<i64>) { c.get(); }",
            "callback" => "pub fn run(f: fn()) { f(); }",
            "ffi" => "pub fn foreign() {} pub fn run() { foreign(); }",
            _ => "pub fn run() {}",
        });
        if mode == "ffi" {
            let crate::parser::ast::Item::Function(f) = &mut worker.items[0] else {
                panic!()
            };
            f.rust_bridge = Some(Box::new(crate::rust_bridge_ir::RustBridgeSymbol::new(
                "foreign".into(),
                vec![],
                crate::rust_bridge_ir::Scalar::Void,
            )));
        }
        let mut queries = EffectQueries::default();
        queries.names.insert("dependency".into(), dependency);
        let mut queries = std::rc::Rc::new(queries);
        if mode != "missing" {
            let mut provider = crate::semantic::TypeChecker::new();
            provider.set_effect_queries(queries.clone(), dependency);
            provider.check_program(&worker);
            assert!(provider.errors.is_empty(), "{mode}: {:?}", provider.errors);
        }
        if mode == "stale" {
            let mut fresh = EffectQueries::default();
            fresh.names.insert("dependency".into(), dependency);
            fresh.reuse_from(&queries);
            queries = std::rc::Rc::new(fresh);
            assert!(queries.contract_facts.borrow().is_empty());
        }
        let source = match mode {
            "waiting" => {
                "import dependency as alias; @effects(no_wait) fn guard(c: BlockingCell<i64>) { alias::run(c); }"
            }
            "callback" => {
                "import dependency as alias; @effects(no_wait) fn guard(f: fn()) { alias::run(f); }"
            }
            _ => "import dependency as alias; @effects(no_wait) fn guard() { alias::run(); }",
        };
        let mut checker = crate::semantic::TypeChecker::new();
        checker.register_module("alias", "dependency", "dependency", &worker);
        checker.set_effect_queries(queries, UnitId::ENTRY);
        checker.check_program(&program(source));
        let codes: Vec<_> = checker.errors.iter().map(|e| e.code).collect();
        let expected = match mode {
            "current" => None,
            "waiting" => Some(ErrorCode::E2610),
            _ => Some(ErrorCode::E2612),
        };
        if let Some(code) = expected {
            assert!(codes.contains(&code), "{mode}: {:?}", checker.errors);
            assert!(
                codes.iter().all(|c| *c == code),
                "{mode}: {:?}",
                checker.errors
            );
        } else {
            assert!(codes.is_empty(), "{mode}: {:?}", checker.errors);
        }
    }
}

#[test]
fn imported_lock_unavailable_evidence_fails_closed() {
    let mut queries = EffectQueries::default();
    queries
        .names
        .insert("dependency".to_owned(), crate::module::ModuleId(7));
    let worker = program("pub fn run() {}");
    let main = program(
        "import dependency as alias; async fn main() { let m = Mutex::new(0); lock m as value { alias::run(); } }",
    );
    let mut checker = crate::semantic::TypeChecker::new();
    checker.register_module("alias", "dependency", "dependency", &worker);
    checker.set_effect_queries(std::rc::Rc::new(queries), UnitId::ENTRY);
    checker.check_program(&main);
    assert!(
        checker
            .errors
            .iter()
            .any(|error| error.code == crate::diagnostics::ErrorCode::E2604)
    );
}

#[test]
fn imported_lock_callable_asyncness_unknown_and_dependency_deduplication() {
    use std::{cell::RefCell, rc::Rc};
    let mut queries = EffectQueries::default();
    let dependency = crate::module::ModuleId(7);
    queries.names.insert("dependency".to_owned(), dependency);
    let imports = HashMap::from([("alias".to_owned(), "dependency".to_owned())]);
    let target = FunctionId::method(TypeId::local("alias"), "run");
    // A resolvable but unavailable declaring unit is synchronous/unknown,
    // so the consumer must record a site and fail closed on absent evidence.
    assert_eq!(
        queries.external_lock_callable(UnitId::ENTRY, &target, &imports),
        Some((dependency, FunctionId::free("run"), false))
    );
    assert!(
        queries
            .effect_evidence(dependency, FunctionId::free("run"))
            .is_none()
    );
    let source = program("pub async fn run() {}");
    let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
    queries
        .complete(dependency, || {
            solve_unit(
                &source,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::from([(FunctionId::free("run"), true)]),
                &HashMap::new(),
                |_| PANIC,
            )
        })
        .unwrap();
    let tracking = Rc::new(RefCell::new(
        super::super::incremental::SyntaxQueries::default(),
    ));
    *queries.tracking.borrow_mut() = Rc::downgrade(&tracking);
    for count in [32, 128, 512] {
        queries.source_reads.borrow_mut().clear();
        for _ in 0..count {
            assert_eq!(
                queries.external_lock_callable(UnitId::ENTRY, &target, &imports),
                Some((dependency, FunctionId::free("run"), true))
            );
        }
        assert_eq!(queries.source_reads.borrow()[&UnitId::ENTRY].len(), 1);
        println!("imported lock: resolutions={count} distinct source dependencies=1");
    }
}

#[test]
fn imported_capabilities_resolve_aliases_and_keep_unknown_units_conservative() {
    let mut queries = EffectQueries::default();
    let dependency = crate::module::ModuleId(7);
    queries.names.insert("dependency".to_owned(), dependency);
    let source = program("fn pure() -> i64 { return 1; } fn output() { println(1); }");
    let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
    let imports = HashMap::from([("alias".to_owned(), "dependency".to_owned())]);
    let target = |name| FunctionId::method(TypeId::local("alias"), name);
    assert_eq!(
        queries.external_waiting(UnitId::ENTRY, &target("pure"), &imports),
        LOCK_EFFECT_WAIT
    );
    let missing = queries.external_capabilities(UnitId::ENTRY, &target("init"), &imports);
    assert!(missing.may_io);
    assert!(missing.runtime.intersects(PANIC));
    queries
        .complete(dependency, || {
            solve_unit(
                &source,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &HashMap::new(),
                |_| PANIC,
            )
        })
        .unwrap();
    assert_eq!(
        queries.external_waiting(UnitId::ENTRY, &target("pure"), &imports),
        RuntimeEffects::NONE
    );
    assert_eq!(
        queries.external_waiting(UnitId::ENTRY, &target("missing"), &imports),
        LOCK_EFFECT_WAIT
    );
    assert!(
        !queries
            .external_capabilities(UnitId::ENTRY, &target("pure"), &imports)
            .may_io
    );
    assert!(
        queries
            .external_capabilities(UnitId::ENTRY, &target("output"), &imports)
            .may_io
    );
    assert!(
        queries
            .external_capabilities(UnitId::ENTRY, &target("missing"), &imports)
            .may_io
    );
}

#[test]
fn static_builtin_io_capabilities_do_not_follow_panic_classification() {
    let queries = EffectQueries::default();
    for (owner, method) in [
        ("fs", "write_string"),
        ("fs", "read_to_string"),
        ("fs", "write_string_async"),
        ("net", "bind"),
        ("net", "write_async"),
        ("parallel", "map"),
    ] {
        let target = FunctionId::method(TypeId::local(owner), method);
        assert!(
            intrinsic_effects(&target).is_some(),
            "known panic summary: {target}"
        );
        assert!(
            queries
                .external_capabilities(UnitId::ENTRY, &target, &HashMap::new())
                .may_io,
            "knowing the panic mask does not prove IO absence: {target}"
        );
    }
    let parse = FunctionId::method(TypeId::local("f64"), "parse");
    assert_eq!(intrinsic_effects(&parse), Some(RuntimeEffects::NONE));
    assert!(
        !queries
            .external_capabilities(UnitId::ENTRY, &FunctionId::free("pow"), &HashMap::new())
            .may_io
    );
}

#[test]
fn static_builtin_io_propagates_through_direct_transitive_and_imported_calls() {
    let mut queries = EffectQueries::default();
    let dependency = crate::module::ModuleId(7);
    queries.names.insert("dependency".into(), dependency);
    let source = program(
        r#"
import std::fs;
import std::net;
import std::fs as files;
fn write() { fs::write_string("unused", "data"); }
fn read_alias() { files::read_to_string("unused"); }
fn listen() { net::bind("127.0.0.1:0"); }
fn relay() { write(); }
fn relay_twice() { relay(); }
fn network_relay() { listen(); }
fn pure() { pow(2, 3); }
"#,
    );
    let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
    assert!(graph.iter().all(|(_, sites)| !sites.has_unknown));
    queries
        .complete(dependency, || {
            solve_unit(
                &source,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &HashMap::new(),
                |target| intrinsic_effects(target).unwrap_or(PANIC),
            )
        })
        .unwrap();
    for name in [
        "write",
        "read_alias",
        "listen",
        "relay",
        "relay_twice",
        "network_relay",
    ] {
        assert!(
            queries
                .effect_capabilities(dependency, FunctionId::free(name))
                .unwrap()
                .may_io,
            "fallback IO solution lost {name}"
        );
        let imported = FunctionId::method(TypeId::local("dependency"), name);
        assert!(
            queries
                .external_capabilities(UnitId::ENTRY, &imported, &HashMap::new())
                .may_io,
            "imported summary lost {name}"
        );
    }
    assert!(
        !queries
            .effect_capabilities(dependency, FunctionId::free("pure"))
            .unwrap()
            .may_io
    );
}

#[test]
fn io_propagates_through_cycles_and_unknown_calls() {
    let queries = EffectQueries::default();
    let source = program(
        "fn a() { b(); } fn b() { a(); println(1); } fn indirect(f: fn() -> i64) -> i64 { return f(); } fn pure() { pow(2, 3); }",
    );
    let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
    queries
        .complete(UnitId::ENTRY, || {
            solve_unit(
                &source,
                &graph,
                &HashMap::<ExprId, Type>::new(),
                None,
                &HashMap::new(),
                &HashMap::new(),
                |_| PANIC,
            )
        })
        .unwrap();
    for name in ["a", "b", "indirect"] {
        assert!(
            queries
                .effect_capabilities(UnitId::ENTRY, FunctionId::free(name))
                .unwrap()
                .may_io
        );
    }
    assert!(
        !queries
            .effect_capabilities(UnitId::ENTRY, FunctionId::free("pure"))
            .unwrap()
            .may_io
    );
}

#[test]
fn output_capabilities_preserve_resolution_and_panic_independence() {
    use std::rc::Rc;
    let dependency = crate::module::ModuleId(7);
    for (imports, output) in [
        ("", "print"),
        ("", "println"),
        ("", "std::io::print"),
        ("", "std::io::println"),
        ("import std::io as out;", "out::print"),
        ("import std::io as out;", "out::println"),
        ("import std::io::print as write;", "write"),
        ("import std::io::println as line;", "line"),
    ] {
        let source = program(&format!(
            "{imports} pub fn emit(n: i64) {{ {output}(n); }} pub fn relay() {{ emit(1); }}"
        ));
        let mut queries = EffectQueries::default();
        queries.names.insert("provider".into(), dependency);
        let queries = Rc::new(queries);
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.set_effect_queries(queries.clone(), dependency);
        checker.check_program(&source);
        assert!(checker.errors.is_empty(), "{output}: {:?}", checker.errors);
        for name in ["emit", "relay"] {
            let local = queries
                .effect_capabilities(dependency, FunctionId::free(name))
                .unwrap();
            assert!(local.may_io, "{output}: {name}");
            assert!(!local.runtime.intersects(ANY_PANIC), "{output}: {name}");
            let imported = queries.external_capabilities(
                UnitId::ENTRY,
                &FunctionId::method(TypeId::local("alias"), name),
                &HashMap::from([("alias".into(), "provider".into())]),
            );
            assert_eq!(*local, imported, "{output}: imported {name}");
        }
    }
    for (source, may_io, panics) in [
        ("fn print(n: i64) {} fn emit() { print(1); }", false, false),
        (
            "fn println(n: i64) {} fn emit() { println(1); }",
            false,
            false,
        ),
        (
            "import std::io::println as line; fn output() { line(2); } fn emit(line: fn(i64)) { line(1); }",
            true,
            true,
        ),
        (
            "class Printer { pub static fn println(n: i64) {} } fn emit() { Printer::println(1); }",
            false,
            false,
        ),
        (
            "fn println(n: i64) { panic(\"user\"); } fn emit() { println(1); }",
            false,
            true,
        ),
        ("fn emit(println: fn(i64)) { println(1); }", true, true),
        ("fn emit(n: i64) { std::io::println(1 / n); }", true, true),
    ] {
        let queries = Rc::new(EffectQueries::default());
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.set_effect_queries(queries.clone(), UnitId::ENTRY);
        checker.check_program(&program(source));
        assert!(checker.errors.is_empty(), "{source}: {:?}", checker.errors);
        let capabilities = queries
            .effect_capabilities(UnitId::ENTRY, FunctionId::free("emit"))
            .unwrap();
        assert_eq!(capabilities.may_io, may_io, "{source}");
        assert_eq!(
            capabilities.runtime.intersects(ANY_PANIC),
            panics,
            "{source}"
        );
    }
}

#[test]
fn output_capability_shared_target_classification_is_bounded() {
    use crate::query_stats::{Counter, Session, count};
    for size in [8, 32, 128] {
        let source = program(&format!(
            "import std::io::println as line; fn emit() {{ {} }} fn relay() {{ {} }}",
            "line(1);".repeat(size),
            "emit();".repeat(size)
        ));
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
        let classifications = std::cell::Cell::new(0);
        let _session = Session::start(true, false);
        let effects = solve_unit(
            &source,
            &graph,
            &HashMap::<ExprId, Type>::new(),
            None,
            &HashMap::new(),
            &HashMap::new(),
            |target| {
                classifications.set(classifications.get() + 1);
                intrinsic_effects(target).unwrap_or(PANIC)
            },
        );
        assert_eq!(classifications.get(), 1);
        assert_eq!(count(Counter::EffectSolve), 1);
        assert_eq!(count(Counter::EffectIoSolve), 0);
        // Inventory and propagation revisit the two unique edges a bounded
        // number of times, independent of the number of source call sites.
        assert!(count(Counter::EffectEdges) <= 6);
        assert!(count(Counter::EffectInventory) <= 20 * size + 20);
        for name in ["emit", "relay"] {
            assert!(effects.may_io.contains(&FunctionId::free(name)), "{name}");
        }
        eprintln!(
            "output sites={size}, helper calls={size}: inventory={}, edges={}, classifications={}",
            count(Counter::EffectInventory),
            count(Counter::EffectEdges),
            classifications.get()
        );
    }
}

#[test]
fn unified_query_reuses_all_facts_and_isolates_units() {
    let queries = EffectQueries::default();
    let source = program("fn first() { first(); }");
    let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
    for unit in [UnitId::ENTRY, crate::module::ModuleId(1)] {
        let facts = queries
            .complete(unit, || {
                solve_unit(
                    &source,
                    &graph,
                    &HashMap::<ExprId, Type<TypeId>>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| PANIC,
                )
            })
            .unwrap();
        assert!(facts.helpers.contains_key(&FunctionId::free("first")));
        assert!(!facts.facts.intersects(&FunctionId::free("first"), PANIC));
        for _ in 0..8 {
            let hit = queries.complete(unit, || panic!("recomputed")).unwrap();
            assert!(Arc::ptr_eq(&facts, &hit));
        }
    }
    assert_eq!(queries.units.stats().computations, 2);
    assert_eq!(queries.units.stats().hits, 16);
}

#[test]
fn recursive_components_guards_and_unknown_calls_keep_their_effects() {
    let program = program(
        "fn a() { b(); } fn b() { a(); } fn danger(n: i64) -> i64 { return 10 / n; } fn caller(n: i64) -> i64 { return danger(n); } fn indirect(f: fn() -> i64) -> i64 { return f(); }",
    );
    let facts = analyze(&program, &[], &HashMap::new(), |_| PANIC);
    assert!(!facts[&FunctionId::free("a")]);
    assert!(!facts[&FunctionId::free("b")]);
    assert!(facts[&FunctionId::free("danger")]);
    assert!(facts[&FunctionId::free("caller")]);
    assert!(facts[&FunctionId::free("indirect")]);
}
#[test]
fn nested_lambda_hazards_belong_to_the_lambda_and_constructors_union() {
    let program = program(
        "fn outer() { let f = || { return 10 / 0; }; } class C { pub init(self) { let n = 10 / 0; } pub init(self, n: i64) {} }",
    );
    let Item::Function(outer) = &program.items[0] else {
        panic!("function");
    };
    let Stmt::Let(binding) = &outer.body.stmts[0] else {
        panic!("binding");
    };
    let Expr::Lambda(lambda) = &binding.init else {
        panic!("lambda");
    };
    let lambda_id = FunctionId::free("lifted_lambda");
    let facts = analyze(&program, &[(lambda_id, lambda)], &HashMap::new(), |_| PANIC);
    assert!(!facts[&FunctionId::free("outer")]);
    assert!(facts[&lambda_id]);
    assert!(facts[&FunctionId::method(TypeId::local("C"), "init")]);
}

#[test]
fn function_field_expression_lambda_inventory_is_linear_and_owned() {
    use crate::query_stats::{Counter, Session, count};
    for nested in [false, true] {
        let mut previous = None;
        for size in [8, 32, 128, 512] {
            let body = if nested {
                format!("let f = {} 1;", "|| ".repeat(size))
            } else {
                (0..size)
                    .map(|i| format!("let callback_{i} = |x: i64| x / x;"))
                    .collect()
            };
            let program = program(&format!("fn root() {{ {body} }}"));
            let mut index = crate::compiler_db::ids::BodyIndex::default();
            index.register_program(&program);
            let Item::Function(root) = &program.items[0] else {
                panic!()
            };
            let _session = Session::start(true, false);
            let direct = scan_direct_effects(
                FunctionId::free("root"),
                root.body.id,
                &root.body,
                &HashMap::<ExprId, Type>::new(),
                Some(&index),
                true.into(),
            );
            assert_eq!(direct.len(), size + 1);
            assert!(!direct[0].panics);
            assert_eq!(
                direct.iter().filter(|body| body.panics).count(),
                if nested { 0 } else { size }
            );
            let visits = count(Counter::EffectInventory);
            if let Some((previous_size, previous_visits)) = previous {
                assert!(visits <= previous_visits * (size / previous_size));
            }
            previous = Some((size, visits));
            println!(
                "expression lambda nested={nested} count={size} direct_bodies={} ast_events={visits}",
                direct.len()
            );
        }
    }
}
