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

#[test]
fn enum_bound_parameter_index_scales_with_parameters_and_payloads() {
    use crate::semantic::ids::TypeId;
    use normalization::HASHED_KEY_STEPS;

    // With no normalization rules, the counted hashes belong only to bound names.
    let tables = PartialCheckerTables::default();
    let (empty_program, _) = checked("");
    HASHED_KEY_STEPS.set(0);
    lower_resolution_partial(&empty_program, &tables);
    let prelude_hashes = HASHED_KEY_STEPS.get();
    for parameter_count in [1, 8, 64] {
        let parameters = (0..parameter_count)
            .map(|i| format!("T{i}"))
            .collect::<Vec<_>>();
        let last = parameters.last().unwrap();
        for occurrences in [1, 8, 128] {
            let variants = (0..occurrences)
                .map(|i| format!("V{i}({last})"))
                .collect::<Vec<_>>();
            let source = format!(
                "enum Family<{}> {{ {} }}",
                parameters.join(", "),
                variants.join(", ")
            );
            let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{errors:?}");
            let expected = parameter_count + occurrences;
            let mut checker = TypeChecker::new();
            HASHED_KEY_STEPS.set(0);
            checker.check_program(&program);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            // Registration and validation each build a scope. A singleton lookup
            // compares its only name directly in the ordered index.
            assert_eq!(
                HASHED_KEY_STEPS.get(),
                if parameter_count == 1 {
                    2
                } else {
                    2 * expected
                },
                "P={parameter_count}, N={occurrences}"
            );
            HASHED_KEY_STEPS.set(0);
            let env = lower_env(&program, &tables);
            assert_eq!(HASHED_KEY_STEPS.get(), expected);
            let variants = &env.enums.map["Family"].variants;
            assert_eq!(variants.len(), occurrences);
            assert!(variants.values().all(|p| p == &[Type::Named(last.clone())]));

            let info = checker.symbols.lookup_enum("Family").unwrap();
            let name = TypeId::local("Family");
            let enums = HashMap::from([(name, info.clone())]);
            let declared = HashSet::from([info.name.clone()]);
            HASHED_KEY_STEPS.set(0);
            let canonical = tables.codegen_enum_info(info, &declared);
            assert_eq!(HASHED_KEY_STEPS.get(), expected);
            let payload = [Type::Named(TypeId::local(last))];
            assert!(
                canonical
                    .variants
                    .iter()
                    .all(|v| v.payload_types == payload)
            );

            // Source declarations and checked metadata both supply HIR snapshots.
            let checked_tables = PartialCheckerTables {
                enums: Some(&enums),
                ..Default::default()
            };
            for (tables, paths) in [(PartialCheckerTables::default(), 1), (checked_tables, 2)] {
                HASHED_KEY_STEPS.set(0);
                let resolution = lower_resolution_partial(&program, &tables);
                assert_eq!(HASHED_KEY_STEPS.get() - prelude_hashes, paths * expected);
                assert!(
                    resolution.enums[&name]
                        .variants
                        .iter()
                        .all(|v| v.payloads == payload)
                );
            }
        }
    }
}

#[test]
fn interface_default_receiver_copies_scale_with_the_final_type() {
    use crate::parser::ast::REBUILT_TYPE_NODES;
    use crate::semantic::ids::SemanticType;

    for leaf in ["i64", "Self", "Alias"] {
        for depth in [0, 4, 16, 32] {
            let payload = (0..depth).fold(leaf.to_owned(), |ty, _| format!("FrozenArray<{ty}>"));
            let mut setup = 0;
            for count in [0, 1, 8, 32] {
                let source = format!(
                    "import std::collections::Map; class Alias {{}} interface I {{
                        fn probe(self, m: Map<i64, {payload}>) {{ {} }}
                     }} class Owner implements I {{}}",
                    "m.len();".repeat(count)
                );
                let (mut program, errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(errors.is_empty(), "{errors:?}");
                assert!(
                    crate::desugar::DesugarPass::run(&mut program, &mut [])
                        .diagnostics
                        .is_empty()
                );
                let mut checker = TypeChecker::new();
                crate::register_prelude(&mut checker).unwrap();
                checker.check_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                if leaf == "Alias" {
                    // The post-rewrite returns an entire tree containing a new Self.
                    checker
                        .normalized_types
                        .insert(Type::Named("Alias".into()), Type::Named("Middle".into()));
                    checker.normalized_types.insert(
                        Type::Named("Middle".into()),
                        Type::Generic("Option".into(), vec![Type::Named("Self".into())]),
                    );
                }
                let tables = SemanticFacts::from_checker(&checker).lowering();
                let Item::Class(owner) = program.items.last().unwrap() else {
                    unreachable!()
                };
                let method = &owner.methods[0];
                assert!(method.is_interface_default);
                let env = lower_env(&program, &tables);
                REBUILT_TYPE_NODES.set(0);
                let function = lower_method(
                    method,
                    &owner.name,
                    None,
                    &env.fn_returns,
                    &env.classes,
                    &env.enums,
                    &tables,
                )
                .unwrap();
                let copies = REBUILT_TYPE_NODES.get();
                if count == 0 {
                    setup = copies;
                    continue;
                }
                let per_method = copies - setup;
                let owner_ty = Type::Named(owner.name.clone());
                let expected_leaf = match leaf {
                    "i64" => Type::I64,
                    "Self" => owner_ty,
                    _ => Type::Generic("Option".into(), vec![owner_ty]),
                };
                let expected_payload = (0..depth).fold(expected_leaf, |ty, _| {
                    Type::Generic("FrozenArray".into(), vec![ty])
                });
                let expected: SemanticType =
                    Type::Generic("Map".into(), vec![Type::I64, expected_payload]).into();
                assert_eq!(function.body.len(), count);
                for stmt in &function.body {
                    let HirStmt::Expr(call) = stmt else {
                        unreachable!()
                    };
                    let HirExprKind::MethodCall {
                        object, builtin, ..
                    } = &call.kind
                    else {
                        unreachable!()
                    };
                    assert_eq!(object.ty, expected);
                    assert_eq!(*builtin, Some(BuiltinMethod::Intrinsic(Intrinsic::MapLen)));
                    assert_eq!(call.ty, Type::I64);
                }
                eprintln!(
                    "default receiver leaf={leaf} depth={depth} calls={count} copied_nodes={per_method}"
                );
                // Storage and HIR conversion each copy the map and its children.
                // Normalization copies scalar leaves and the whole Alias rule output.
                let storage = depth + 3;
                let (hir, normalized_leaves) = match leaf {
                    "i64" => (storage, 2),
                    "Self" => (storage, 1),
                    _ => (storage + 1, 3),
                };
                let len_result = 3; // storage, normalization and HIR for i64
                assert_eq!(
                    per_method,
                    count * (storage + normalized_leaves + hir + len_result)
                );
            }
        }
    }
}

#[test]
fn checked_map_length_hashes_each_type_node_linearly() {
    use crate::parser::ast::HASHED_TYPE_NODES;

    for depth in [0, 4, 16, 32, 64] {
        let payload = (0..depth).fold("i64".to_owned(), |inner, _| format!("FrozenArray<{inner}>"));
        for count in [1, 8, 32] {
            let (program, mut checker) = checked(&format!(
                "import std::collections::Map; fn probe(m: Map<i64, {payload}>) {{ {} }}",
                "m.len();".repeat(count)
            ));
            // First exercise the populated table from the original measurement.
            let mut payload = Type::I64;
            for _ in 0..depth {
                payload = Type::Generic("FrozenArray".into(), vec![payload]);
                checker
                    .normalized_types
                    .insert(payload.clone(), payload.clone());
            }
            checker
                .normalized_types
                .insert(Type::Named("Alias".into()), Type::String);
            let expected_receiver: crate::semantic::ids::SemanticType =
                Type::Generic("Map".into(), vec![Type::I64, payload]).into();
            for compound_keys in [false, true] {
                if compound_keys {
                    // Every payload prefix is a live, exact subkey of another rule.
                    let mut payload = Type::I64;
                    for _ in 0..depth {
                        payload = Type::Generic("FrozenArray".into(), vec![payload]);
                        checker.normalized_types.insert(
                            Type::Generic("Envelope".into(), vec![payload.clone()]),
                            Type::Named("Specialization".into()),
                        );
                    }
                }
                normalization::HASHED_KEY_STEPS.set(0);
                let facts = SemanticFacts::from_checker(&checker);
                let tables = facts.lowering();
                assert!(std::ptr::eq(
                    tables.normalized_types.index().unwrap(),
                    facts.normalized_types.index().unwrap(),
                ));
                let index_hashes = normalization::HASHED_KEY_STEPS.get();
                let env = lower_env(&program, &tables);
                let Item::Function(f) = program.items.last().unwrap() else {
                    unreachable!()
                };
                let mut ctx = LowerCtx::new(&env.fn_returns, &env.classes, &env.enums, &tables);
                ctx.bind("m".into(), ctx.normalize(&f.params[0].ty));
                HASHED_TYPE_NODES.set(0);
                normalization::HASHED_KEY_STEPS.set(0);
                for stmt in &f.body.stmts {
                    let Stmt::Expr(stmt) = stmt else {
                        unreachable!()
                    };
                    let call = lower_expr(&stmt.expr, &mut ctx).unwrap();
                    let HirExprKind::MethodCall {
                        builtin, object, ..
                    } = &call.kind
                    else {
                        panic!("expected method")
                    };
                    assert_eq!(*builtin, Some(BuiltinMethod::Intrinsic(Intrinsic::MapLen)));
                    assert_eq!(object.ty, expected_receiver);
                    assert_eq!(call.ty, Type::I64);
                }
                let visits = HASHED_TYPE_NODES.get();
                // Map, key, payload leaf, D wrappers and scalar call result.
                let nodes = count * (depth + 4);
                let probes = normalization::HASHED_KEY_STEPS.get();
                println!(
                    "checked map length: compounds={compound_keys} depth={depth} count={count} hash_visits={visits} key_hashes={probes} index_hashes={index_hashes}"
                );
                assert_eq!(
                    visits, 0,
                    "full-tree hash at depth {depth} for {count} calls"
                );
                assert!(
                    (1..=4 * nodes).contains(&probes),
                    "{probes} shallow hashes for {nodes} nodes"
                );
            }
        }
    }
}

#[test]
fn composite_receivers_build_only_their_final_types() {
    use crate::parser::ast::REBUILT_TYPE_NODES;
    use crate::parser::iter::{AstEvent, AstWalk};
    use crate::semantic::ids::SemanticType;

    for depth in [0, 4, 16, 32] {
        let payload = (0..depth).fold("i64".to_owned(), |ty, _| format!("FrozenArray<{ty}>"));
        let map = format!("Map<i64, {payload}>");
        for count in [1, 8, 32] {
            for receiver in [
                "(flag ? left : right)",
                "[left, right]",
                "[left, right][0]",
                "frozen[0]",
                "(match flag { true => left, false => right })",
                "identity(left)",
                "callback(left)",
                "captured(left)",
            ] {
                let capture = if receiver == "captured(left)" {
                    "let mut captured = callback; let writer = || { captured = callback; };"
                } else {
                    ""
                };
                let (program, checker) = checked(&format!(
                    "import std::collections::{{Array, Map}};
                     fn identity(x: {map}) -> {map} {{ return x; }}
                     fn probe(flag: bool, left: {map}, right: {map}, maps: Array<{map}>, frozen: FrozenArray<{map}>, callback: fn({map}) -> {map}) {{ {capture} {} }}",
                    format!("{receiver}.len();").repeat(count)
                ));
                let f = program
                    .items
                    .iter()
                    .find_map(|item| match item {
                        Item::Function(f) if f.name == "probe" => Some(f),
                        _ => None,
                    })
                    .unwrap();
                let facts = SemanticFacts::from_checker(&checker);
                let checked_tables = facts.lowering();
                let standalone = PartialCheckerTables::default();
                for tables in [&checked_tables, &standalone] {
                    let env = lower_env(&program, tables);
                    let mut ctx = LowerCtx::new(&env.fn_returns, &env.classes, &env.enums, tables);
                    for p in &f.params {
                        ctx.bind(p.name.clone(), ctx.normalize(&p.ty));
                    }
                    if receiver == "captured(left)" {
                        let callback = ctx.lookup("callback").unwrap().ty.clone();
                        ctx.bind_cell("captured".to_owned(), callback, true);
                    }
                    let receivers: Vec<_> = AstWalk::new(AstEvent::Block(&f.body))
                        .filter_map(|event| match event {
                            AstEvent::Expr(Expr::MethodCall(m)) => Some(&m.object),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(receivers.len(), count);
                    REBUILT_TYPE_NODES.set(0);
                    if tables.checked {
                        for receiver in &receivers {
                            for event in AstWalk::new(AstEvent::Expr(receiver)) {
                                if let AstEvent::Expr(e) = event {
                                    let fact = ctx.expr_type(&e.id()).unwrap();
                                    let _: SemanticType = ctx.normalize(&fact).into();
                                }
                            }
                            if let Expr::Call(c) = receiver
                                && c.callee == "captured"
                            {
                                // The cell operand and its loaded callable own one signature each.
                                let binding = ctx.lookup("captured").unwrap();
                                let _: SemanticType = (&binding.ty).into();
                                let _: SemanticType = (&binding.ty).into();
                            }
                            if let Expr::Match(m) = receiver {
                                // The HIR owns the arm annotations separately from their expressions.
                                for arm in &m.arms {
                                    let crate::parser::ast::MatchBody::Expr(e) = &arm.body else {
                                        unreachable!()
                                    };
                                    let _ = checker.expr_types[&e.id()].clone();
                                }
                                // Structural pattern binding uses the scrutinee's source type.
                                let _ = checker.expr_types[&m.scrutinee.id()].clone();
                            }
                        }
                    }
                    let required = REBUILT_TYPE_NODES.get();
                    let expected: SemanticType = ctx
                        .normalize(&checker.expr_types[&receivers[0].id()].array_storage_type())
                        .into();
                    REBUILT_TYPE_NODES.set(0);
                    for expr in receivers {
                        let hir = lower_expr(expr, &mut ctx).unwrap();
                        assert_eq!(hir.ty, expected, "{receiver}");
                        if receiver == "captured(left)" {
                            assert!(matches!(hir.kind, HirExprKind::Invoke { .. }));
                        }
                    }
                    let actual = REBUILT_TYPE_NODES.get();
                    if tables.checked {
                        assert!(
                            actual <= required,
                            "{receiver} depth={depth} count={count}: {actual} > {required}"
                        );
                    }
                    println!(
                        "shape={receiver} depth={depth} count={count} checked={} actual={actual} required={required}",
                        tables.checked
                    );
                }
            }
        }
    }
}

#[test]
fn checked_map_receivers_build_only_their_final_types() {
    use crate::parser::ast::REBUILT_TYPE_NODES;
    use crate::parser::iter::{AstEvent, AstWalk};
    use crate::semantic::ids::SemanticType;

    for depth in [0, 4, 16, 32] {
        let payload = (0..depth).fold("i64".to_owned(), |ty, _| format!("FrozenArray<{ty}>"));
        let map = format!("Map<i64, {payload}>");
        for count in [1, 8, 64] {
            let (program, checker) = checked(&format!(
                "import std::collections::Map;
                 open class Base {{ pub map: {map}; }} class Holder extends Base {{}}
                 fn probe(m: {map}, holder: Holder) {{ {} }}",
                "m.len(); holder.map.len();".repeat(count)
            ));
            let f = program
                .items
                .iter()
                .find_map(|item| match item {
                    Item::Function(f) => Some(f),
                    _ => None,
                })
                .unwrap();
            let facts = SemanticFacts::from_checker(&checker);
            let checked_tables = facts.lowering();
            let standalone = PartialCheckerTables::default();
            for tables in [&checked_tables, &standalone] {
                let env = lower_env(&program, tables);
                let mut ctx = LowerCtx::new(&env.fn_returns, &env.classes, &env.enums, tables);
                for p in &f.params {
                    ctx.bind(p.name.clone(), ctx.normalize(&p.ty));
                }
                let map_ty: SemanticType = ctx.normalize(&f.params[0].ty).into();
                for field in [false, true] {
                    let receivers: Vec<_> = AstWalk::new(AstEvent::Block(&f.body))
                        .filter_map(|event| match event {
                            AstEvent::Expr(Expr::MethodCall(m))
                                if matches!(m.object, Expr::FieldAccess(..)) == field =>
                            {
                                Some(&m.object)
                            }
                            _ => None,
                        })
                        .collect();
                    assert_eq!(receivers.len(), count);
                    // Reference cost for taking each fact into its final HIR slot,
                    // including the holder local in a field access. Excludes methods.
                    REBUILT_TYPE_NODES.set(0);
                    if tables.checked {
                        for receiver in &receivers {
                            for event in AstWalk::new(AstEvent::Expr(receiver)) {
                                if let AstEvent::Expr(e) = event {
                                    let fact = ctx.expr_type(&e.id()).unwrap();
                                    let _: SemanticType = ctx.normalize(&fact).into();
                                }
                            }
                        }
                    }
                    let required = REBUILT_TYPE_NODES.get();
                    REBUILT_TYPE_NODES.set(0);
                    MEMBER_LOOKUP_STEPS.set(0);
                    for receiver in receivers {
                        let hir = lower_expr(receiver, &mut ctx).unwrap();
                        assert_eq!(hir.ty, map_ty);
                        assert_eq!(matches!(hir.kind, HirExprKind::FieldAccess { .. }), field);
                    }
                    let copies = REBUILT_TYPE_NODES.get();
                    let steps = MEMBER_LOOKUP_STEPS.get();
                    if tables.checked {
                        assert!(
                            copies <= required,
                            "depth={depth} calls={count} field={field}: {copies} > {required}"
                        );
                    }
                    let expected_steps = if field && !tables.checked {
                        2 * count
                    } else {
                        0
                    };
                    assert_eq!(steps, expected_steps);
                    println!(
                        "depth={depth} calls={count} checked={} field={field} nodes={copies} required={required} visits={steps}",
                        tables.checked
                    );
                }
            }
        }
    }
}

#[test]
fn checked_map_get_lowers_the_result_type_once() {
    use crate::parser::ast::REBUILT_TYPE_NODES;

    for depth in [0, 4, 16, 32] {
        let payload = (0..depth).fold("i64".to_owned(), |ty, _| format!("FrozenArray<{ty}>"));
        for count in [1, 8, 64] {
            // Identical receivers, arguments and declarations isolate the extra
            // nodes in get's Option<V> result relative to contains's bool.
            let work = |method: &str, intrinsic: Intrinsic| {
                let (program, checker) = checked(&format!(
                    "import std::collections::Map; fn measure(m: Map<i64, {payload}>) {{ {} }}",
                    format!("m.{method}(1);").repeat(count)
                ));
                let function = program
                    .items
                    .iter()
                    .find_map(|item| match item {
                        Item::Function(f) => Some(f),
                        _ => None,
                    })
                    .unwrap();
                let facts = SemanticFacts::from_checker(&checker);
                REBUILT_TYPE_NODES.set(0);
                let hir = lower_function(
                    function,
                    &HashMap::new(),
                    &Classes::default(),
                    &Enums::default(),
                    &facts.lowering(),
                )
                .unwrap();
                let copies = REBUILT_TYPE_NODES.get();
                let body = &hir.body;
                assert_eq!(body.len(), count);
                for stmt in body {
                    let HirStmt::Expr(call) = stmt else {
                        panic!("expected method call");
                    };
                    let HirExprKind::MethodCall {
                        builtin, object, ..
                    } = &call.kind
                    else {
                        panic!("expected method call");
                    };
                    assert_eq!(*builtin, Some(BuiltinMethod::Intrinsic(intrinsic)));
                    if intrinsic == Intrinsic::MapGet {
                        let Type::Generic(_, args) = &object.ty else {
                            panic!("expected map");
                        };
                        assert_eq!(
                            builtin_types::unary_arg(&call.ty, B::Option),
                            Some(&args[1])
                        );
                    } else {
                        assert_eq!(call.ty, Type::Bool);
                    }
                }
                copies
            };
            let get = work("get", Intrinsic::MapGet);
            let contains = work("contains", Intrinsic::MapContains);
            // The checked storage type and the final HIR type each account
            // for one pass over the additional result nodes.
            assert!(
                get <= contains + 2 * count * (depth + 1),
                "depth={depth} calls={count} get={get} contains={contains}"
            );
            println!("depth={depth} calls={count} get={get} contains={contains}");
        }
    }
}

#[test]
fn checked_map_lengths_skip_declaration_and_reference_site_work() {
    use crate::ir::lowered::{DECLARED_METHOD_LOOKUPS, LirInst, LirRvalue};
    use crate::semantic::ids::METHOD_INTERN_REQUESTS;
    use crate::semantic::intrinsics::Intrinsic;

    for count in [1, 8, 64, 256] {
        for receiver in ["m", "(b ? m : other)"] {
            let (program, checker) = checked(&format!(
                "import std::collections::Map; fn measure(m: Map<i64, i64>, other: Map<i64,i64>, b: bool) {{ {} }}",
                format!("{receiver}.len();").repeat(count)
            ));
            let (hir, errors) =
                lower_program_with(&program, &SemanticFacts::from_checker(&checker));
            assert!(errors.is_empty(), "{errors:?}");
            DECLARED_METHOD_LOOKUPS.set(0);
            METHOD_INTERN_REQUESTS.set(0);
            let lir = crate::ir::lowered::lower_program(&hir);
            assert_eq!(DECLARED_METHOD_LOOKUPS.get(), 0, "{count} calls");
            assert_eq!(METHOD_INTERN_REQUESTS.get(), 0, "{count} calls");
            let lengths = lir
                .functions
                .iter()
                .flat_map(|f| &f.blocks)
                .flat_map(|b| &b.instrs)
                .filter(|i| {
                    matches!(
                        i,
                        LirInst::Compute {
                            value: LirRvalue::IntrinsicCall {
                                intrinsic: Intrinsic::MapLen,
                                ..
                            },
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(lengths, count);
        }
    }
}

#[test]
fn declared_method_keeps_argument_coercion_and_reference_site() {
    use crate::ir::lowered::{DECLARED_METHOD_LOOKUPS, LirInst, LirRvalue};
    use crate::semantic::ids::{FunctionId, METHOD_INTERN_REQUESTS, TypeId};

    let (program, checker) = checked(
        "interface View { fn id(self) -> i64; }
        class Item implements View { pub fn id(self) -> i64 { return 1; } }
        class Sink { pub fn len(self, item: View, n: &mut i64) { n = item.id(); } }
        fn run(s: Sink, item: Item) { let mut n = 0; s.len(item, &n); }",
    );
    let (hir, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
    assert!(errors.is_empty(), "{errors:?}");
    DECLARED_METHOD_LOOKUPS.set(0);
    METHOD_INTERN_REQUESTS.set(0);
    let lir = crate::ir::lowered::lower_program(&hir);
    assert!(DECLARED_METHOD_LOOKUPS.get() > 0);
    assert!(METHOD_INTERN_REQUESTS.get() > 0);
    let wanted = FunctionId::method(TypeId::local("Sink"), "len");
    let run = lir
        .functions
        .iter()
        .find(|f| f.name.is_free_named("run"))
        .unwrap();
    let operations: Vec<_> = run
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter_map(|i| match i {
            LirInst::Compute {
                value: LirRvalue::PrepareMethod { .. },
                ..
            } => Some("prepare"),
            LirInst::Compute {
                value: LirRvalue::BeginReferenceCall,
                ..
            } => Some("begin"),
            LirInst::Compute {
                value:
                    LirRvalue::Coerce {
                        target: crate::semantic::ids::SemanticType::Named(t),
                        ..
                    },
                ..
            } if *t == TypeId::local("View") => Some("coerce"),
            LirInst::Compute {
                value:
                    LirRvalue::ReferenceDebug {
                        callee, index: 1, ..
                    },
                ..
            } if *callee == wanted => Some("reference"),
            LirInst::Compute {
                value: LirRvalue::MethodCall { method, args, .. },
                ..
            } if method == "len"
                && matches!(
                    args.as_slice(),
                    [_, crate::ir::lowered::LirOperand::Reference { .. }]
                ) =>
            {
                Some("call")
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        operations,
        ["prepare", "begin", "coerce", "reference", "call"]
    );
}

#[test]
fn builtin_dispatch_resolves_once_per_call() {
    use crate::semantic::intrinsics::{METHOD_LOOKUPS, RECEIVER_LOOKUPS};
    // Parse the shared prelude before measuring the workload, including on a cold test process.
    checked("");
    let mut fixed = (0, 0, 0);
    for n in [0, 8, 64, 256] {
        let body =
            "ch.close(); c.recv(); observe(o.map(|v| v + 1).is_some(), r.map(|v| v + 1).is_ok());"
                .repeat(n);
        let source = format!(
            "class C {{ pub fn recv(self) -> i64 {{ return 7; }} }} fn observe(a: bool, b: bool) {{}} fn work(ch: Channel<i64>, c: C, o: Option<i64>, r: Result<i64, String>) {{ {body} }}"
        );
        METHOD_LOOKUPS.with(|count| count.set(0));
        RECEIVER_LOOKUPS.with(|count| count.set(0));
        let (program, checker) = checked(&source);
        let checked_lookups = METHOD_LOOKUPS.with(|count| count.get());
        let resolutions = RECEIVER_LOOKUPS.with(|count| count.get());
        if n == 0 {
            fixed = (checked_lookups, checker.builtin_calls.len(), resolutions);
        }
        assert_eq!(checked_lookups, fixed.0 + n * 6);
        assert_eq!(resolutions, fixed.2 + n * 6);
        let recorded = checker.builtin_calls.len() - fixed.1;
        assert_eq!(recorded, 5 * n);
        let report = crate::semantic::ConcurrencyAnalyzer::new()
            .check_program_with_helpers(
                &program,
                &crate::compiler_db::HelperSummary::new(),
                &checker.builtin_calls,
            )
            .report;
        assert_eq!(report.channel_operations, n);
        let (hir, errors) = lower_program_with(&program, &SemanticFacts::from_checker(&checker));
        assert!(errors.is_empty(), "{errors:?}");
        let lir = crate::ir::lowered::lower_program(&hir);
        let instructions: usize = lir
            .functions
            .iter()
            .flat_map(|f| &f.blocks)
            .map(|b| b.instrs.len())
            .sum();
        use crate::ir::lowered::{LirInst, LirRvalue};
        use crate::semantic::intrinsics::AdtMethod;
        let mut adt_sites = [0; 3];
        for instr in lir
            .functions
            .iter()
            .flat_map(|f| &f.blocks)
            .flat_map(|b| &b.instrs)
        {
            if let LirInst::Compute {
                value: LirRvalue::EnumMethod { method, .. },
                ..
            } = instr
            {
                match method {
                    AdtMethod::Map => adt_sites[0] += 1,
                    AdtMethod::IsSome => adt_sites[1] += 1,
                    AdtMethod::IsOk => adt_sites[2] += 1,
                    _ => panic!("unexpected Option/Result instruction: {method}"),
                }
            }
        }
        assert_eq!(adt_sites, [2 * n, n, n]);
        assert_eq!(METHOD_LOOKUPS.with(|count| count.get()), checked_lookups);
        assert_eq!(RECEIVER_LOOKUPS.with(|count| count.get()), resolutions);
        println!(
            "sites={} classifications={} receiver_lookups={} downstream_lookups=0 builtin_facts={recorded} channel_sites={n} lir_instructions={instructions}",
            6 * n,
            checked_lookups - fixed.0,
            resolutions - fixed.2
        );
    }
}
