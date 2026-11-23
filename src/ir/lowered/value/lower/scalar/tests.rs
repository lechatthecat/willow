use super::*;
use crate::ir::lowered::{
    Builder, CLASS_FIELD_ENUMERATIONS, DECLARED_METHOD_LOOKUPS, tests::checked_hir,
};
use crate::ir::typed_ast::{BINDING_INDEX_ENTRIES, HirStmt};
use crate::parser::ast::REBUILT_TYPE_NODES;

fn with_probe(
    source: &str,
    check: impl for<'e> FnOnce(&'e HirExpr, &mut ScalarLowering<'_, 'e, '_>),
) {
    let hir = checked_hir(source);
    let f = hir
        .functions
        .iter()
        .find(|f| f.name.is_free_named("probe"))
        .unwrap();
    let HirStmt::Return {
        value: Some(root), ..
    } = &f.body[0]
    else {
        panic!("probe must return the test expression");
    };
    let declarations = LoweringDeclarations::new(&hir.resolution);
    let mut builder = Builder::new(&f.params, false, &declarations);
    let callables = Callables {
        signatures: hir
            .functions
            .iter()
            .map(|f| {
                (
                    f.name,
                    CallSignature {
                        params: CallParameters::Source(&f.params),
                        output: &f.return_type,
                        is_async: f.is_async,
                    },
                )
            })
            .collect(),
        ..Callables::new(&declarations)
    };
    let mut ctx = ScalarLowering {
        emitted: &mut Vec::new(),
        locals: &mut builder.locals,
        names: &builder.local_by_name,
        callables: &callables,
        is_async: false,
        snapshots: true,
        values: HashMap::new(),
        reference_sites: HashMap::new(),
        plan: &mut ScalarPlan::default(),
        work: vec![Work::Enter(root)],
    };
    check(root, &mut ctx);
}

fn validate<'e>(root: &'e HirExpr, ctx: &mut ScalarLowering<'_, 'e, '_>) {
    assert!(scalar(
        root,
        ctx.names,
        ctx.locals,
        ctx.callables,
        false,
        ctx.plan,
    ));
    ctx.snapshots = ctx.plan.snapshots;
}

fn step<'e>(ctx: &mut ScalarLowering<'_, 'e, '_>) {
    let work = ctx.work.pop().expect("pending scalar action");
    assert!(ctx.step(work).is_none());
}

#[test]
fn named_call_validation_keeps_only_distinct_local_slots() {
    for calls in [1, 8, 32] {
        for local in [false, true] {
            let param = if local { "f: fn() -> i64" } else { "" };
            let items = vec!["f()"; calls].join(",");
            let source = format!(
                "fn f() -> i64 {{ return 7; }} fn probe({param}) -> Array<i64> {{ return [{items}]; }}"
            );
            with_probe(&source, |root, ctx| {
                validate(root, ctx);
                assert_eq!(ctx.plan.local_callees.len(), usize::from(local));
                while !ctx.work.is_empty() {
                    step(ctx);
                }
                let call_count = ctx
                    .emitted
                    .iter()
                    .filter(|inst| match inst {
                        SourceInst::Compute {
                            value: LirRvalue::DirectCall { callee, .. },
                            ..
                        } => !local && callee.is_free_named("f"),
                        SourceInst::Compute {
                            value: LirRvalue::IndirectCall { .. },
                            ..
                        } => local,
                        _ => false,
                    })
                    .count();
                assert_eq!(call_count, calls);
                assert_eq!(ctx.names.len(), usize::from(local));
            });
        }
    }
}

#[test]
fn pending_scalar_call_coercions_only_copy_the_callee_snapshot() {
    for layers in [0, 8] {
        let t = (0..layers).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        let target = format!("View<{t}>");
        for depth in [1, 8, 32] {
            for indirect in [false, true] {
                let local = if indirect {
                    format!(", lift: fn({target}) -> Leaf")
                } else {
                    String::new()
                };
                let calls = format!("{}x{}", "lift(".repeat(depth), ")".repeat(depth));
                with_probe(
                    &format!(
                        "interface View<T> {{}} class Leaf implements {target} {{}}
                     fn lift(x: {target}) -> Leaf {{ return new Leaf(); }}
                     fn probe(x: Leaf{local}) -> Leaf {{ return {calls}; }}"
                    ),
                    |root, ctx| {
                        validate(root, ctx);
                        let initial_locals = ctx.locals.len();
                        REBUILT_TYPE_NODES.set(0);
                        for _ in 0..depth {
                            step(ctx);
                        }
                        let snapshots = if indirect { depth } else { 0 };
                        assert_eq!(ctx.locals.len(), initial_locals + snapshots);
                        // Callable root, View, its array layers and i64 leaf, and Leaf result.
                        assert_eq!(REBUILT_TYPE_NODES.get(), snapshots * (layers + 4));
                        assert_eq!(ctx.work.len(), 2 * depth + 1);
                        for (i, frame) in ctx.work[..2 * depth].chunks_exact(2).enumerate() {
                            match frame {
                                [
                                    Work::Finish(_, Finish::LocalCall { snapshot, .. }),
                                    Work::CoerceSnapshot {
                                        snapshot: target_local,
                                        index: 0,
                                        ..
                                    },
                                ] => {
                                    assert!(indirect);
                                    assert_eq!(snapshot, target_local);
                                    assert_eq!(snapshot.0 as usize, initial_locals + i);
                                }
                                [
                                    Work::Finish(_, Finish::Call { callee, .. }),
                                    Work::Coerce {
                                        target: Cow::Borrowed(ty),
                                        ..
                                    },
                                ] => {
                                    assert!(!indirect);
                                    assert!(std::ptr::eq(
                                        *ty,
                                        ctx.callables.signatures[callee]
                                            .params
                                            .iter()
                                            .next()
                                            .unwrap()
                                    ));
                                }
                                _ => panic!("expected a call and its parameter conversion"),
                            }
                        }
                        while !ctx.work.is_empty() {
                            step(ctx);
                        }
                        let conversions = ctx
                            .emitted
                            .iter()
                            .filter(|inst| {
                                matches!(
                                    inst,
                                    SourceInst::Compute {
                                        value: LirRvalue::Coerce { .. },
                                        ..
                                    }
                                )
                            })
                            .count();
                        assert_eq!(conversions, depth);
                    },
                );
            }
        }
    }
}

#[test]
fn scalar_interface_coercion_materializes_the_validated_parameter_at_emission() {
    for layers in [0, 8, 32] {
        let t = (0..layers).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        with_probe(
            &format!(
                "interface View<T> {{}} class Leaf implements View<{t}> {{}}
             interface Sink<T> {{ fn take(self, x: T) -> i64; }}
             fn probe(s: Sink<View<{t}>>, x: Leaf) -> i64 {{ return s.take(x); }}"
            ),
            |root, ctx| {
                let HirExprKind::MethodCall {
                    object,
                    method,
                    args,
                    ..
                } = &root.kind
                else {
                    panic!("expected method call")
                };
                let resolution = ctx.callables.resolution;
                let sig =
                    crate::ir::lowered::method_signature(resolution, &object.ty, method).unwrap();
                REBUILT_TYPE_NODES.set(0);
                let expected = [sig
                    .interface
                    .as_ref()
                    .unwrap()
                    .bindings()
                    .specialize(&sig.declaration.params[0])];
                assert_eq!(REBUILT_TYPE_NODES.get(), layers + 2);
                assert!(argument_coercible(&expected[0], &args[0], resolution));
                assert_eq!(REBUILT_TYPE_NODES.get(), layers + 2);

                DECLARED_METHOD_LOOKUPS.set(0);
                REBUILT_TYPE_NODES.set(0);
                validate(root, ctx);
                step(ctx);
                assert_eq!(DECLARED_METHOD_LOOKUPS.get(), 1);
                assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                assert!(ctx.work.iter().any(|w| matches!(w, Work::CoerceProjected { target, .. } if target.bindings.matches(target.param, &expected[0]))));
                while !ctx.work.is_empty() {
                    step(ctx);
                }
            },
        );
    }
}

#[test]
fn identity_validation_and_scheduling_borrow_the_selected_operand() {
    for layers in [0, 8, 32] {
        let t = (0..layers).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        for (comparison, target_on_left) in [("v == x", true), ("x != v", false)] {
            with_probe(
                &format!(
                    "interface View<T> {{}} class Leaf implements View<{t}> {{}}
                 fn probe(x: Leaf, v: View<{t}>) -> bool {{ return {comparison}; }}"
                ),
                |root, ctx| {
                    let HirExprKind::Binary { lhs, rhs, .. } = &root.kind else {
                        panic!("expected identity comparison")
                    };
                    let resolution = ctx.callables.resolution;
                    REBUILT_TYPE_NODES.set(0);
                    let target = if resolution.can_coerce(&rhs.ty, &lhs.ty) {
                        &lhs.ty
                    } else {
                        assert!(resolution.can_coerce(&lhs.ty, &rhs.ty));
                        &rhs.ty
                    };
                    assert_eq!(target_on_left, std::ptr::eq(target, &lhs.ty));
                    let compatibility = REBUILT_TYPE_NODES.get();
                    REBUILT_TYPE_NODES.set(0);
                    validate(root, ctx);
                    assert_eq!(REBUILT_TYPE_NODES.get(), compatibility);
                    step(ctx);
                    assert_eq!(REBUILT_TYPE_NODES.get(), compatibility);
                    assert!(ctx.work.iter().any(|w| matches!(w, Work::Coerce { target: Cow::Borrowed(ty), .. } if std::ptr::eq(*ty, target))));
                    while !ctx.work.is_empty() {
                        step(ctx);
                    }
                    assert!(ctx.emitted.iter().any(|i| matches!(i, SourceInst::Compute {value: LirRvalue::Binary { operand_ty, .. }, ..} if operand_ty == target)));
                },
            );
        }
    }
}

#[test]
fn inherited_and_static_calls_keep_their_validated_parameters() {
    for call in ["obj.member(n)", "Base::associated(n)"] {
        with_probe(
            &format!(
                "open class Base {{
                pub fn member(self, n: i64) -> i64 {{ return n; }}
                pub static fn associated(n: i64) -> i64 {{ return n; }}
             }}
             open class Mid extends Base {{}} class Leaf extends Mid {{}}
             fn probe(obj: Leaf, n: i64) -> i64 {{ return {call}; }}"
            ),
            |root, ctx| {
                DECLARED_METHOD_LOOKUPS.set(0);
                REBUILT_TYPE_NODES.set(0);
                validate(root, ctx);
                step(ctx);
                assert_eq!(DECLARED_METHOD_LOOKUPS.get(), 1);
                assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                while !ctx.work.is_empty() {
                    step(ctx);
                }
                assert_eq!(DECLARED_METHOD_LOOKUPS.get(), 1);
            },
        );
    }
}

#[test]
fn memberwise_stores_reuse_the_base_first_layout() {
    for depth in [2, 8, 32] {
        let mut source = String::from("open class L0 { pub f0: i64; } ");
        for level in 1..depth {
            source.push_str(&format!(
                "open class L{level} extends L{} {{ pub f{level}: i64; }} ",
                level - 1
            ));
        }
        let class = format!("L{}", depth - 1);
        let values = (0..depth)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let ctor = format!("new {class}({values})");
        source.push_str(&format!(
            "fn probe() -> Array<{class}> {{ return [{ctor},{ctor},{ctor}]; }}"
        ));
        with_probe(&source, |root, ctx| {
            CLASS_FIELD_ENUMERATIONS.set(0);
            validate(root, ctx);
            while !ctx.work.is_empty() {
                step(ctx);
            }
            assert_eq!(CLASS_FIELD_ENUMERATIONS.get(), 1);
            let fields: Vec<_> = ctx
                .emitted
                .iter()
                .filter_map(|inst| match inst {
                    SourceInst::Compute {
                        value: LirRvalue::FieldStore { field, .. },
                        ..
                    } => Some(field.as_str()),
                    _ => None,
                })
                .collect();
            let expected: Vec<_> = (0..depth).map(|i| format!("f{i}")).collect();
            assert_eq!(fields.len(), 3 * depth);
            for stores in fields.chunks_exact(depth) {
                assert_eq!(stores, expected);
            }
        });
    }
}

#[test]
fn formatted_calls_parse_each_template_once() {
    use crate::interpolate::SPEC_PARSES;
    for builtin in ["format", "panic"] {
        for repeats in [1, 8, 32] {
            let template = " item={{ {} {:.6f} }}".repeat(repeats);
            let args = ", n, x".repeat(repeats);
            with_probe(
                &format!(
                    "fn probe(n: i64, x: f64) -> String {{ return {builtin}(\"{template}\"{args}); }}"
                ),
                |root, ctx| {
                    SPEC_PARSES.set(0);
                    validate(root, ctx);
                    step(ctx);
                    assert_eq!(SPEC_PARSES.get(), 1);
                    let mut raised = false;
                    while let Some(work) = ctx.work.pop() {
                        if ctx.step(work).is_some() {
                            raised = true;
                            break;
                        }
                    }
                    assert_eq!(
                        ctx.emitted
                            .iter()
                            .filter(|inst| matches!(
                                inst,
                                SourceInst::Compute {
                                    value: LirRvalue::FormatScalar { .. },
                                    ..
                                }
                            ))
                            .count(),
                        2 * repeats
                    );
                    assert_eq!(raised, builtin == "panic");
                    assert_eq!(SPEC_PARSES.get(), 1);
                },
            );
        }
    }
}

#[test]
fn sibling_format_literals_borrow_source_until_emission() {
    use crate::interpolate::{DECODED_LITERAL_BYTES, SPEC_PARSES};
    for count in [1, 8, 32] {
        for length in [4, 256, 1024] {
            let prefix = "é".repeat(length);
            let fmt = format!("format(\"{prefix}{{{{x}}}}={{}}!\", n)");
            for diverges in [false, true] {
                let first = if diverges {
                    "pass(panic(\"stop\")),"
                } else {
                    ""
                };
                let calls = vec![fmt.as_str(); count].join(",");
                with_probe(
                    &format!(
                        "fn pass(x: String) -> String {{ return x; }}
                    fn probe(n: i64) -> Array<String> {{ return [{first}{calls}]; }}"
                    ),
                    |root, ctx| {
                        SPEC_PARSES.set(0);
                        DECODED_LITERAL_BYTES.set(0);
                        validate(root, ctx);
                        assert_eq!(ctx.plan.formats.as_ref().unwrap().len(), count);
                        assert_eq!(DECODED_LITERAL_BYTES.get(), 0);
                        let mut emitted_bytes = 0;
                        let mut raised = false;
                        while let Some(action) = ctx.work.pop() {
                            let before = ctx.emitted.len();
                            raised = ctx.step(action).is_some();
                            for inst in &ctx.emitted[before..] {
                                if let SourceInst::Compute {
                                    value: LirRvalue::StringLiteral(text),
                                    ..
                                } = inst
                                    && text != "stop"
                                {
                                    emitted_bytes += text.len();
                                }
                            }
                            assert_eq!(DECODED_LITERAL_BYTES.get(), emitted_bytes);
                            if raised {
                                break;
                            }
                        }
                        assert_eq!(raised, diverges);
                        assert_eq!(
                            emitted_bytes,
                            if diverges {
                                0
                            } else {
                                count * (prefix.len() + "{x}=!".len())
                            }
                        );
                        assert_eq!(SPEC_PARSES.get(), count);
                    },
                );
            }
        }
    }
}

pub(super) fn type_nodes(ty: &Type) -> usize {
    let mut stack = vec![ty];
    let mut nodes = 0;
    while let Some(ty) = stack.pop() {
        nodes += 1;
        let (args, tail) = ty.child_parts();
        stack.extend(args.iter().chain(tail));
    }
    nodes
}

fn owned_pending_nodes(ctx: &ScalarLowering<'_, '_, '_>) -> usize {
    let planned = ctx
        .plan
        .coercions
        .iter()
        .flat_map(|params| params.values())
        .filter_map(|p| match p {
            Parameter::Declared(Cow::Owned(ty)) => Some(ty),
            Parameter::Declared(Cow::Borrowed(_))
            | Parameter::Projected(_)
            | Parameter::ResultArgument => None,
        });
    let queued = ctx.work.iter().filter_map(|w| match w {
        Work::Coerce {
            target: Cow::Owned(ty),
            ..
        } => Some(ty),
        _ => None,
    });
    planned.chain(queued).map(type_nodes).sum()
}

#[test]
fn sibling_interface_coercions_borrow_their_targets_until_emission() {
    for layers in [0, 8, 32] {
        let inner = (0..layers).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        for count in [1, 8, 32] {
            for diverges in [false, true] {
                let first = if diverges {
                    "pass(panic(\"stop\")),"
                } else {
                    ""
                };
                let calls = vec!["s.take(x)"; count].join(",");
                with_probe(&format!("interface View<T> {{}} class Leaf implements View<{inner}> {{}}
                    interface Sink<T> {{ fn take(self, x: T) -> i64; }}
                    fn pass(n: i64) -> i64 {{ return n; }}
                    fn probe(s: Sink<View<{inner}>>, x: Leaf) -> Array<i64> {{ return [{first}{calls}]; }}"), |root, ctx| {
                    let HirExprKind::Array { elements } = &root.kind else { panic!("expected array") };
                    let HirExprKind::MethodCall {object, method, args, ..} = &elements.last().unwrap().kind else { panic!("expected method") };
                    let resolution = ctx.callables.resolution;
                    let sig = crate::ir::lowered::method_signature(resolution,&object.ty,method).unwrap();
                    let expected = [sig.interface.as_ref().unwrap().bindings().specialize(&sig.declaration.params[0])];
                    REBUILT_TYPE_NODES.set(0);
                    assert!(argument_coercible(&expected[0], &args[0], resolution));
                    assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                    PEAK_MATERIALIZED_TARGET_NODES.set(0);
                    validate(root, ctx);
                    assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                    let mut peak = 0;
                    let mut raised = false;
                    loop {
                        peak = peak.max(owned_pending_nodes(ctx));
                        if raised { break; }
                        let Some(action) = ctx.work.pop() else { break };
                        raised = ctx.step(action).is_some();
                    }
                    assert_eq!(peak, 0);
                    assert_eq!(PEAK_MATERIALIZED_TARGET_NODES.get(), if diverges { 0 } else { layers + 2 });
                    assert_eq!(raised, diverges);
                    let emitted = ctx.emitted.iter().filter(|i| matches!(i, SourceInst::Compute { value: LirRvalue::Coerce { target, .. }, .. } if target == &expected[0])).count();
                    assert_eq!(emitted, if diverges { 0 } else { count });
                });
            }
        }
    }
}

#[test]
fn enum_validation_only_substitutes_used_payload_types() {
    use crate::ir::typed_ast::{HirEnumInfo, HirEnumVariant};
    let class = TypeId::local("Packet");
    let param = TypeId::local("T");
    let mut hir = crate::ir::typed_ast::HirResolution::default();
    hir.enums.insert(
        class,
        HirEnumInfo {
            type_params: [param].into_iter().collect(),
            variants: [
                ("Empty", vec![]),
                ("Number", vec![Type::I64]),
                ("Data", vec![Type::Named(param)]),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (name, payloads))| HirEnumVariant {
                name: name.into(),
                tag: i as i64,
                payloads,
            })
            .collect(),
        },
    );
    let declarations = LoweringDeclarations::new(&hir);
    let callables = Callables::new(&declarations);
    let arg = HirExpr {
        kind: HirExprKind::Int(1),
        ty: Type::I64,
        span: Span::dummy(),
    };
    for depth in [0, 8, 64] {
        let nested = (0..depth).fold(Type::I64, |ty, _| Type::Array(Box::new(ty)));
        let ty = Type::Generic(class, vec![nested]);
        REBUILT_TYPE_NODES.set(0);
        assert!(enum_variant_supported(
            &class,
            "Empty",
            &ty,
            &[],
            &callables
        ));
        assert!(enum_variant_supported(
            &class,
            "Number",
            &ty,
            std::slice::from_ref(&arg),
            &callables
        ));
        assert!(!enum_variant_supported(
            &class,
            "Missing",
            &ty,
            &[],
            &callables
        ));
        assert_eq!(REBUILT_TYPE_NODES.get(), 0);
    }
    let void = Type::Generic(class, vec![Type::Void]);
    assert!(enum_variant_supported(
        &class,
        "Data",
        &void,
        &[],
        &callables
    ));
    assert!(!enum_variant_supported(
        &class,
        "Data",
        &void,
        &[arg],
        &callables
    ));
}

#[test]
fn scalar_visits_each_node_once_and_snapshots_each_store_operand_independently() {
    use crate::ir::typed_ast::POSTORDER_VISITS;
    for width in [2, 8, 32] {
        let sum = vec!["n"; width].join("+");
        with_probe(
            &format!("fn probe(n: i64) -> i64 {{ return {sum}; }}"),
            |root, ctx| {
                let mut expr = root.clone();
                POSTORDER_VISITS.set(0);
                lower(
                    &mut expr,
                    ctx.emitted,
                    ctx.locals,
                    ctx.names,
                    ctx.callables,
                    false,
                );
                assert_eq!(POSTORDER_VISITS.get(), 2 * width - 1);
                assert!(!ctx.emitted.iter().any(|i| matches!(
                    i,
                    SourceInst::Compute {
                        value: LirRvalue::Use(_),
                        ..
                    }
                )));
            },
        );
    }
    for (index, value) in [("id(n)", "n+n"), ("n+n", "id(n)")] {
        with_probe(
            &format!(
                "fn id(x: i64) -> i64 {{return x;}}
            fn probe(a: Array<i64>, n: i64) -> i64 {{ return a[{index}] + ({value}); }}"
            ),
            |root, ctx| {
                let HirExprKind::Binary { lhs, rhs, .. } = &root.kind else {
                    panic!("expected binary")
                };
                let HirExprKind::Index { array, index } = &lhs.kind else {
                    panic!("expected index")
                };
                let store = SourceInst::IndexAssign {
                    array: (**array).clone(),
                    index: (**index).clone(),
                    value: (**rhs).clone(),
                };
                let n = ctx.names["n"];
                POSTORDER_VISITS.set(0);
                assert!(lower_store(
                    &store,
                    ctx.emitted,
                    ctx.locals,
                    ctx.names,
                    ctx.callables,
                    false
                ));
                assert_eq!(POSTORDER_VISITS.get(), 6);
                assert_eq!(ctx.emitted.iter().filter(|i| matches!(i, SourceInst::Compute { value: LirRvalue::Use(LirOperand::Local(source)), .. } if *source == n)).count(), 1);
            },
        );
    }
}

#[test]
fn coercions_share_only_the_bindings_used_by_each_parameter() {
    with_probe(
        "open class Base {} class Leaf extends Base {}
        interface Sink<T,U> { fn take(self, x: T, y: U) -> i64; }
        fn probe(a: Sink<Base,i64>, b: Sink<Base,bool>, x: Leaf) -> i64 {
            return a.take(x, 1) + b.take(x, true);
        }",
        |root, ctx| {
            validate(root, ctx);
            let coercions = ctx.plan.coercions.as_ref().unwrap();
            assert_eq!(coercions.len(), 2);
            let mut targets = coercions.values().map(|parameter| {
                let Parameter::Projected(target) = parameter else {
                    panic!("expected specialization")
                };
                target
            });
            let (a, b) = (targets.next().unwrap(), targets.next().unwrap());
            assert!(std::ptr::eq(a.param, b.param));
            assert!(std::ptr::eq(a.bindings.type_params, b.bindings.type_params));
            assert!(
                a.bindings
                    .matches(a.param, &Type::Named(TypeId::local("Base")))
            );
            assert!(
                b.bindings
                    .matches(b.param, &Type::Named(TypeId::local("Base")))
            );
            while !ctx.work.is_empty() {
                step(ctx);
            }
        },
    );
}

#[test]
fn exact_interface_arguments_validate_without_rebuilding_bindings() {
    for layers in [0, 8, 32] {
        let t = (0..layers).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        for count in [1, 8, 32] {
            let calls = vec!["s.take(x, wrapped)"; count].join(",");
            with_probe(
                &format!(
                    "interface View<T> {{}}
                interface Sink<T> {{ fn take(self, x: T, wrapped: View<T>) -> i64; }}
                fn probe(s: Sink<{t}>, x: {t}, wrapped: View<{t}>) -> Array<i64> {{
                    return [{calls}];
                }}"
                ),
                |root, ctx| {
                    REBUILT_TYPE_NODES.set(0);
                    DECLARED_METHOD_LOOKUPS.set(0);
                    assert!(scalar(
                        root,
                        ctx.names,
                        ctx.locals,
                        ctx.callables,
                        false,
                        ctx.plan,
                    ));
                    assert_eq!(DECLARED_METHOD_LOOKUPS.get(), count);
                    assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                    assert!(ctx.plan.coercions.is_none());
                    while !ctx.work.is_empty() {
                        step(ctx);
                    }
                    assert_eq!(
                        ctx.emitted
                            .iter()
                            .filter(|i| matches!(
                                i,
                                SourceInst::Compute {
                                    value: LirRvalue::MethodCall { .. },
                                    ..
                                }
                            ))
                            .count(),
                        count
                    );
                },
            );
        }
    }
}

#[test]
fn sibling_format_validation_and_emission_allocate_no_descriptor_vectors() {
    use crate::interpolate::SPEC_PARSES;
    use crate::ir::lowered::PEAK_FORMAT_DESCRIPTOR_CAPACITY as PEAK;
    for width in [2, 8, 64] {
        let call = format!("format(\"{}\"{})", "{}!".repeat(width), ", n".repeat(width));
        for count in [1, 8, 32] {
            for diverges in [false, true] {
                let first = if diverges {
                    "pass(panic(\"stop\")),"
                } else {
                    ""
                };
                let calls = vec![call.as_str(); count].join(",");
                with_probe(
                    &format!(
                        "fn pass(x: String) -> String {{ return x; }}
                    fn probe(n: i64) -> Array<String> {{ return [{first}{calls}]; }}"
                    ),
                    |root, ctx| {
                        SPEC_PARSES.set(0);
                        crate::interpolate::RENDERED_TEMPLATE_BYTES.set(0);
                        PEAK.set(0);
                        validate(root, ctx);
                        assert_eq!(SPEC_PARSES.get(), count);
                        assert_eq!(crate::interpolate::RENDERED_TEMPLATE_BYTES.get(), 0);
                        assert_eq!(PEAK.get(), 0);
                        let mut stopped = false;
                        while let Some(action) = ctx.work.pop() {
                            if ctx.step(action).is_some() {
                                stopped = true;
                                break;
                            }
                        }
                        assert_eq!(stopped, diverges);
                        assert_eq!(
                            crate::interpolate::RENDERED_TEMPLATE_BYTES.get(),
                            if diverges {
                                0
                            } else {
                                count * width * "{}!".len()
                            }
                        );
                        assert_eq!(SPEC_PARSES.get(), count);
                        assert_eq!(PEAK.get(), 0);
                    },
                );
            }
        }
    }
}

#[test]
fn rejected_formats_emit_no_prefix_bytes_instructions_or_locals() {
    for bytes in [1, 256, 8192] {
        for diverges in [false, true] {
            let prefix = "p".repeat(bytes);
            let first = if diverges {
                format!("pass(panic(\"{prefix}\"))")
            } else {
                format!("pass(\"{prefix}\")")
            };
            with_probe(
                &format!("fn pass(x: String) -> String {{ return x; }}
                    fn probe(n: i64) -> Array<String> {{ return [{first}, format(\"tail={{}}\", n)]; }}"),
                |root, ctx| {
                    ctx.emit(LirRvalue::Use(LirOperand::Int(7)), Type::I64, root.span);
                    for template in ["{:.17g}", "{:invalid}", "{}{}"] {
                        let mut invalid = root.clone();
                        let HirExprKind::Array { elements } = &mut invalid.kind else { panic!("expected array") };
                        let HirExprKind::Call { args, .. } = &mut elements[1].kind else { panic!("expected format") };
                        args[0].kind = HirExprKind::Str(template.into());
                        // The array operand would copy and store the prefix before the stored value runs.
                        let store = SourceInst::IndexAssign {
                            array: HirExpr { ty: root.ty.clone(), span: root.span, kind: HirExprKind::Array { elements: vec![elements[0].clone()] } },
                            index: HirExpr { kind: HirExprKind::Int(0), ty: Type::I64, span: root.span },
                            value: elements[1].clone(),
                        };
                        let original = invalid.clone();
                        let names = ctx.names.clone();
                        let locals = ctx.locals.clone();
                        let instructions = ctx.emitted.clone();
                        for is_store in [false, true] {
                            EMITTED_COMPUTES.set(0);
                            EMITTED_LITERAL_BYTES.set(0);
                            RESERVED_LOCALS.set(0);
                            if is_store {
                                assert!(!lower_store(&store, ctx.emitted, ctx.locals, ctx.names, ctx.callables, false));
                            } else {
                                lower(&mut invalid, ctx.emitted, ctx.locals, ctx.names, ctx.callables, false);
                                assert_eq!(invalid, original);
                            }
                            assert_eq!((EMITTED_COMPUTES.get(), EMITTED_LITERAL_BYTES.get(), RESERVED_LOCALS.get()), (0, 0, 0));
                            assert_eq!(*ctx.names, names);
                            assert_eq!(*ctx.locals, locals);
                            assert_eq!(*ctx.emitted, instructions);
                        }
                    }
                    // The same prefix is reached in the accepted program, including before its panic.
                    let mut valid = root.clone();
                    EMITTED_COMPUTES.set(0);
                    EMITTED_LITERAL_BYTES.set(0);
                    RESERVED_LOCALS.set(0);
                    lower(&mut valid, ctx.emitted, ctx.locals, ctx.names, ctx.callables, false);
                    assert_eq!(EMITTED_LITERAL_BYTES.get(), bytes + if diverges { 0 } else { "tail=".len() });
                    assert!(EMITTED_COMPUTES.get() > 0 && RESERVED_LOCALS.get() > 0);
                },
            );
        }
    }
}

#[test]
fn composite_enum_payload_validation_borrows_exact_and_independent_types() {
    for depth in [1, 8, 32] {
        let wrap = |base: &str| (0..depth).fold(base.to_owned(), |ty, _| format!("Array<{ty}>"));
        let generic = wrap("T");
        let concrete = wrap("i64");
        for count in [1, 8, 32] {
            for variant in ["Generic", "Fixed"] {
                let calls = vec![format!("Packet::{variant}(x)"); count].join(",");
                let source = format!(
                    "enum Packet<T> {{ Generic({generic}), Fixed({concrete}) }}
                    fn probe(x: {concrete}) -> Array<Packet<i64>> {{ return [{calls}]; }}"
                );
                with_probe(&source, |root, ctx| {
                    REBUILT_TYPE_NODES.set(0);
                    validate(root, ctx);
                    assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                    while !ctx.work.is_empty() {
                        step(ctx);
                    }
                    let stores = ctx
                        .emitted
                        .iter()
                        .filter(|i| {
                            matches!(
                                i,
                                SourceInst::Compute {
                                    value: LirRvalue::EnumPayloadStore { .. },
                                    ..
                                }
                            )
                        })
                        .count();
                    assert_eq!(stores, count);
                });
            }
        }
    }
}

#[test]
fn divergent_format_operand_does_not_materialize_its_suffix() {
    use crate::interpolate::{RENDERED_TEMPLATE_BYTES, SPEC_PARSES};
    for bytes in [1, 256, 8192] {
        let source = format!("fn pass(x: String) -> String {{ return x; }}
            fn probe() -> String {{ return format(\"head{{}}{{}}{}\", pass(panic(\"stop\")), 42); }}", "s".repeat(bytes));
        with_probe(&source, |root, ctx| {
            RENDERED_TEMPLATE_BYTES.set(0);
            SPEC_PARSES.set(0);
            validate(root, ctx);
            assert_eq!(SPEC_PARSES.get(), 1);
            assert_eq!(RENDERED_TEMPLATE_BYTES.get(), 0);
            let mut raised = false;
            while let Some(action) = ctx.work.pop() {
                if ctx.step(action).is_some() {
                    raised = true;
                    break;
                }
            }
            assert!(raised);
            assert_eq!(RENDERED_TEMPLATE_BYTES.get(), "head{}".len());
            assert_eq!(SPEC_PARSES.get(), 1);
        });
    }
}

#[test]
fn composite_enum_coercion_checks_borrowed_payloads() {
    for depth in [1, 8, 32] {
        let inner = (0..depth).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        let source = format!(
            "interface View<T> {{}} class Leaf implements View<{inner}> {{}}
            enum Packet<T> {{ Generic(View<T>), Fixed(View<{inner}>) }}
            fn probe(x: Leaf) -> Packet<{inner}> {{ return Packet::Fixed(x); }}"
        );
        with_probe(&source, |root, ctx| {
            let HirExprKind::StaticCall { class, args, .. } = &root.kind else {
                panic!("expected enum construction")
            };
            let target = &ctx.callables.resolution.enums[class].variants[1].payloads[0];
            REBUILT_TYPE_NODES.set(0);
            assert!(ctx.callables.resolution.can_coerce(&args[0].ty, target));
            let compatibility = REBUILT_TYPE_NODES.get();
            assert_eq!(compatibility, 0);
            for count in [1, 8, 32] {
                for variant in ["Fixed", "Generic"] {
                    REBUILT_TYPE_NODES.set(0);
                    for _ in 0..count {
                        assert!(enum_variant_supported(
                            class,
                            variant,
                            &root.ty,
                            args,
                            ctx.callables
                        ));
                    }
                    assert_eq!(REBUILT_TYPE_NODES.get(), count * compatibility);
                }
            }
        });
    }
}

#[test]
fn diverging_scalar_arguments_skip_distinct_specializations() {
    for depth in [0, 8, 32] {
        for count in [1, 8, 32] {
            for composite in [false, true] {
                for raising_calls in [0, 1, count] {
                    let raises = raising_calls != 0;
                    let parameter = if composite { "View<T>" } else { "T" };
                    let mut source = format!(
                        "interface View<T> {{}} interface Sink<T> {{ fn take(self, x: {parameter}) -> i64; }}"
                    );
                    let mut params = Vec::new();
                    let mut calls = Vec::new();
                    for i in 0..count {
                        let ty = (0..depth).fold(format!("Tag{i}"), |t, _| format!("Array<{t}>"));
                        source.push_str(&format!(
                            " class Tag{i} {{}} class Leaf{i} implements View<{ty}> {{}}"
                        ));
                        let receiver_arg = if composite { ty } else { format!("View<{ty}>") };
                        params.push(format!("s{i}: Sink<{receiver_arg}>, x{i}: Leaf{i}"));
                        let arg = if i < raising_calls {
                            String::from(r#"panic("stop")"#)
                        } else {
                            format!("x{i}")
                        };
                        calls.push(format!("s{i}.take({arg})"));
                    }
                    source.push_str(&format!(
                        " fn probe({}) -> Array<i64> {{ return [{}]; }}",
                        params.join(","),
                        calls.join(",")
                    ));
                    with_probe(&source, |root, ctx| {
                        let mut peak = owned_pending_nodes(ctx);
                        PEAK_MATERIALIZED_TARGET_NODES.set(0);
                        REBUILT_TYPE_NODES.set(0);
                        validate(root, ctx);
                        assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                        assert_eq!(
                            ctx.plan.coercions.as_ref().map_or(0, HashMap::len),
                            count - raising_calls
                        );
                        assert_eq!(PEAK_MATERIALIZED_TARGET_NODES.get(), 0);
                        peak = peak.max(owned_pending_nodes(ctx));
                        let mut stopped = false;
                        while let Some(action) = ctx.work.pop() {
                            stopped = ctx.step(action).is_some();
                            peak = peak.max(owned_pending_nodes(ctx));
                            if stopped {
                                break;
                            }
                        }
                        assert_eq!(stopped, raises);
                        assert_eq!(peak, 0);
                        assert_eq!(
                            PEAK_MATERIALIZED_TARGET_NODES.get(),
                            if raises { 0 } else { depth + 2 }
                        );
                        let mut conversions = 0;
                        let mut output_target_nodes = 0;
                        let mut panics = 0;
                        for inst in ctx.emitted.iter() {
                            if let SourceInst::Compute { local, value, .. } = inst {
                                match value {
                                    LirRvalue::Coerce { target, .. } => {
                                        conversions += 1;
                                        output_target_nodes += type_nodes(target)
                                            + type_nodes(&ctx.locals[local.0 as usize].ty);
                                    }
                                    LirRvalue::Panic { .. } => panics += 1,
                                    _ => {}
                                }
                            }
                        }
                        assert_eq!(panics, usize::from(raises));
                        assert_eq!(conversions, if raises { 0 } else { count });
                        assert_eq!(output_target_nodes, 2 * conversions * (depth + 2));
                    });
                }
            }
        }
    }
}

#[test]
fn enum_binding_index_is_lazy_and_reused_across_constructions() {
    use crate::ir::typed_ast::{HirEnumInfo, HirEnumVariant};
    let class = TypeId::local("Packet");
    let arg = HirExpr {
        kind: HirExprKind::Int(7),
        ty: Type::I64,
        span: Span::dummy(),
    };
    for arity in [1, 8, 32] {
        for count in [1, 8, 32] {
            let names: Vec<_> = (0..arity).map(|i| TypeId::local(format!("T{i}"))).collect();
            let mut hir = crate::ir::typed_ast::HirResolution::default();
            hir.enums.insert(
                class,
                HirEnumInfo {
                    variants: vec![
                        HirEnumVariant {
                            name: "Fixed".into(),
                            tag: 0,
                            payloads: vec![Type::I64],
                        },
                        HirEnumVariant {
                            name: "Bound".into(),
                            tag: 1,
                            payloads: vec![Type::Named(names[arity - 1])],
                        },
                    ],
                    type_params: names.into_iter().collect(),
                },
            );
            let declarations = LoweringDeclarations::new(&hir);
            let callables = Callables::new(&declarations);
            let mut args = vec![Type::Bool; arity];
            let other = Type::Generic(class, args.clone());
            args[arity - 1] = Type::I64;
            let exact = Type::Generic(class, args);
            BINDING_INDEX_ENTRIES.set(0);
            for _ in 0..count {
                assert!(enum_variant_supported(
                    &class,
                    "Fixed",
                    &exact,
                    std::slice::from_ref(&arg),
                    &callables
                ));
            }
            assert_eq!(BINDING_INDEX_ENTRIES.get(), 0);
            for _ in 0..count {
                assert!(enum_variant_supported(
                    &class,
                    "Bound",
                    &exact,
                    std::slice::from_ref(&arg),
                    &callables
                ));
                assert!(!enum_variant_supported(
                    &class,
                    "Bound",
                    &other,
                    std::slice::from_ref(&arg),
                    &callables
                ));
            }
            assert_eq!(
                BINDING_INDEX_ENTRIES.get(),
                if arity == 1 { 0 } else { arity }
            );
        }
    }
}

#[test]
fn scalar_panic_resolution_preserves_shadowing_and_the_empty_hir_form() {
    with_probe(
        r#"fn probe(panic: fn(String) -> i64) -> i64 { return panic("ok"); }"#,
        |root, ctx| {
            validate(root, ctx);
            assert!(ctx.plan.builtin_panic.is_none());
            while !ctx.work.is_empty() {
                step(ctx);
            }
            assert!(ctx.emitted.iter().any(|inst| matches!(
                inst,
                SourceInst::Compute {
                    value: LirRvalue::IndirectCall { .. },
                    ..
                }
            )));
        },
    );
    with_probe(
        r#"interface Sink<T> { fn take(self, x: T) -> i64; }
        fn probe(s: Sink<Array<i64>>) -> i64 { return s.take(panic("stop")); }"#,
        |root, ctx| {
            // The checker requires a message in source. Lowering also accepts the
            // no-argument HIR form and supplies its default message.
            let mut expr = root.clone();
            let HirExprKind::MethodCall { args, .. } = &mut expr.kind else {
                panic!("expected method")
            };
            let HirExprKind::Call { args, .. } = &mut args[0].kind else {
                panic!("expected panic")
            };
            args.clear();
            let mut plan = ScalarPlan::default();
            REBUILT_TYPE_NODES.set(0);
            assert!(scalar(
                &expr,
                ctx.names,
                ctx.locals,
                ctx.callables,
                false,
                &mut plan,
            ));
            assert_eq!(REBUILT_TYPE_NODES.get(), 0);
            assert!(plan.coercions.is_none());
            assert!(
                lower_validated(
                    &expr,
                    ctx.emitted,
                    ctx.locals,
                    ctx.names,
                    ctx.callables,
                    false,
                    &mut plan
                )
                .is_err()
            );
            assert!(ctx.emitted.iter().any(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::StringLiteral(text), .. } if text == "explicit panic")));
        },
    );
}

#[test]
fn interface_binding_indices_are_lazy_and_shared_by_declarations() {
    for arity in [1, 8, 32] {
        let names = (0..arity)
            .map(|i| format!("T{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut types = vec!["bool"; arity];
        types[arity - 1] = "i64";
        for count in [1, 8, 32] {
            for dependent in [false, true] {
                let parameter = if dependent {
                    format!("T{}", arity - 1)
                } else {
                    "i64".into()
                };
                let calls = vec!["s.take(x)"; count].join(",");
                let source = format!(
                    "interface Sink<{names}> {{ fn take(self, x: {parameter}) -> i64; }}
                    fn probe(s: Sink<{}>, x: i64) -> Array<i64> {{ return [{calls}]; }}",
                    types.join(",")
                );
                with_probe(&source, |root, ctx| {
                    let resolution =
                        crate::ir::typed_ast::HirResolution::clone(ctx.callables.resolution);
                    let class = TypeId::local("Sink");
                    assert_eq!(
                        ctx.callables.resolution.interfaces[&class]
                            .type_params
                            .as_slice()
                            .len(),
                        arity
                    );
                    BINDING_INDEX_ENTRIES.set(0);
                    DECLARED_METHOD_LOOKUPS.set(0);
                    validate(root, ctx);
                    let expected = if dependent && arity > 1 { arity } else { 0 };
                    assert_eq!(BINDING_INDEX_ENTRIES.get(), expected);
                    assert_eq!(DECLARED_METHOD_LOOKUPS.get(), count);
                    while !ctx.work.is_empty() {
                        step(ctx);
                    }
                    assert_eq!(BINDING_INDEX_ENTRIES.get(), expected);
                    // A cloned declaration snapshot sees the same lazy index,
                    // even when it was cloned before the first named lookup.
                    let params = &resolution.interfaces[&class].type_params;
                    assert_eq!(
                        params.position(&TypeId::local(format!("T{}", arity - 1))),
                        Some(arity - 1)
                    );
                    assert_eq!(
                        BINDING_INDEX_ENTRIES.get(),
                        if arity > 1 { arity } else { 0 }
                    );
                });
            }
        }
    }
}

#[test]
fn construction_lookups_scalar_dispatch_empty_and_missing_variants() {
    crate::ir::lowered::tests::check_empty_and_missing_variants(|resolution, expr| {
        scalar(
            expr,
            &HashMap::new(),
            &[],
            &Callables::new(resolution),
            false,
            &mut ScalarPlan::default(),
        )
    });
}

#[test]
fn inherited_method_cache_scalar_misses() {
    crate::ir::lowered::tests::check_inherited_method_misses(|resolution, expr| {
        scalar(
            expr,
            &HashMap::from([("x".to_owned(), LirLocalId(0))]),
            &[],
            &Callables::new(resolution),
            false,
            &mut ScalarPlan::default(),
        )
    });
}
