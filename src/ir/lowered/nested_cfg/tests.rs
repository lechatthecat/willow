use super::*;
use crate::parser::ast::REBUILT_TYPE_NODES;

#[test]
fn inherited_calls_resolve_once_before_their_operands() {
    for count in [1, 8, 32] {
        let mut source = String::from(
            "open class Base {
                pub fn member(self, n: i64) -> i64 { return n; }
                pub static fn associated(n: i64) -> i64 { return n; }
            }
            open class Mid extends Base {} class Leaf extends Mid {}
            fn probe(obj: Leaf, flag: bool) -> Array<i64> { return [",
        );
        let pair = "obj.member(flag ? 1 : 2), Base::associated(flag ? 3 : 4)";
        source.push_str(&vec![pair; count].join(","));
        source.push_str("]; }");
        let hir = super::super::tests::checked_hir(&source);
        let f = hir
            .functions
            .iter()
            .find(|f| f.name.is_free_named("probe"))
            .unwrap();
        let HirStmt::Return {
            value: Some(root), ..
        } = &f.body[0]
        else {
            panic!("expected return");
        };
        let resolution = LoweringDeclarations::new(&hir.resolution);
        let mut builder = Builder::new(&f.params, false, &resolution);
        DECLARED_METHOD_LOOKUPS.set(0);
        lower(&mut builder, root).expect("nested calls");
        assert_eq!(DECLARED_METHOD_LOOKUPS.get(), 2 * count);
        let instructions = || {
            builder
                .blocks
                .iter()
                .flat_map(|(instructions, _)| instructions)
        };
        assert_eq!(
            instructions()
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
        assert_eq!(
            instructions()
                .filter(|i| matches!(
                    i,
                    SourceInst::Compute {
                        value: LirRvalue::StaticCall { .. },
                        ..
                    }
                ))
                .count(),
            count
        );
    }
}

#[test]
fn wide_enum_stores_borrow_metadata_before_a_propagating_payload() {
    for depth in [0, 8] {
        let tail_ty = (0..depth).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        for width in [2, 8, 32, 128] {
            let mut payloads = vec!["i64"; width];
            payloads[width - 1] = "T";
            let mut args = vec!["0"; width];
            args[0] = "first?";
            args[width - 1] = "tail";
            let hir = super::super::tests::checked_hir(&format!(
                "enum Bundle<T> {{ Pieces({}) }}
                 fn probe(first: Option<i64>, tail: {tail_ty}) -> Option<i64> {{
                     let bundle: Bundle<{tail_ty}> = Bundle::Pieces({});
                     return Option::Some(1);
                 }}",
                payloads.join(","),
                args.join(",")
            ));
            let f = hir
                .functions
                .iter()
                .find(|f| f.name.is_free_named("probe"))
                .unwrap();
            let HirStmt::Let { value: root, .. } = &f.body[0] else {
                panic!("expected enum initializer");
            };
            let resolution = LoweringDeclarations::new(&hir.resolution);
            let mut builder = Builder::new(&f.params, false, &resolution);
            builder.return_type = f.return_type.clone();
            let mut ctx = NestedCfg::new(&mut builder, root);
            REBUILT_TYPE_NODES.set(0);
            let action = ctx.pending.pop().unwrap();
            ctx.step(action).unwrap();
            // Local declaration, allocation rvalue and result expression each own the shell type.
            assert_eq!(
                REBUILT_TYPE_NODES.get(),
                3 * (depth + 2),
                "width {width}, depth {depth}"
            );
            assert_eq!(ctx.pending.len(), 2 * width);
            for work in ctx.pending.iter().step_by(2) {
                let Work::EnumStore(_, _, _, _, ty, _) = work else {
                    panic!("expected store");
                };
                assert!(std::ptr::eq(*ty, &root.ty));
            }

            while let Some(work) = ctx.pending.pop() {
                ctx.step(work).unwrap();
            }
            let stores = |block: usize| {
                ctx.builder.blocks[block]
                    .0
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
                    .count()
            };
            let Some(SourceTerminator::Branch {
                then_block,
                else_block,
                ..
            }) = &ctx.builder.blocks[0].1
            else {
                panic!("first payload must branch before any store")
            };
            assert_eq!(stores(0), 0);
            assert_eq!(stores(else_block.0), 0);
            assert!(matches!(
                ctx.builder.blocks[else_block.0].1,
                Some(SourceTerminator::Return(_))
            ));
            assert_eq!(stores(then_block.0), width);
        }
    }
}

fn returning_probe(functions: &[HirFunction]) -> (&HirFunction, &HirExpr) {
    let function = functions
        .iter()
        .find(|f| f.name.is_free_named("probe"))
        .unwrap();
    let HirStmt::Return {
        value: Some(root), ..
    } = &function.body[0]
    else {
        panic!("probe must return its test expression");
    };
    (function, root)
}

#[test]
fn pending_calls_only_copy_the_required_callee_snapshots() {
    for type_depth in [0, 8] {
        let ty = (0..type_depth).fold(String::from("i64"), |ty, _| format!("Array<{ty}>"));
        for depth in [1, 8, 32] {
            for indirect in [false, true] {
                let local_param = if indirect {
                    format!(", call: fn({ty}) -> {ty}")
                } else {
                    String::new()
                };
                let arg = format!("{}flag ? x : x{}", "call(".repeat(depth), ")".repeat(depth));
                let hir = super::super::tests::checked_hir(&format!(
                    "fn call(x: {ty}) -> {ty} {{ return x; }}
                     fn probe(x: {ty}, flag: bool{local_param}) -> {ty} {{ return {arg}; }}"
                ));
                let (f, root) = returning_probe(&hir.functions);
                let resolution = LoweringDeclarations::new(&hir.resolution);
                let mut builder = Builder::new(&f.params, false, &resolution);
                let mut ctx = NestedCfg::new(&mut builder, root);
                let initial_locals = ctx.builder.locals.len();

                if indirect {
                    let HirExprKind::Call { callee, .. } = &root.kind else {
                        panic!("expected named call")
                    };
                    let source = ctx.builder.local_by_name["call"];
                    REBUILT_TYPE_NODES.set(0);
                    assert_eq!(
                        ctx.schedule_indirect_call(&root.span, *callee, &[], source),
                        Some(false)
                    );
                    assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                    assert_eq!(ctx.builder.locals.len(), initial_locals);
                    assert_eq!(ctx.pending.len(), 1);
                }
                REBUILT_TYPE_NODES.set(0);
                for _ in 0..depth {
                    let work = ctx.pending.pop().unwrap();
                    ctx.step(work).unwrap();
                }
                let snapshots = if indirect { depth } else { 0 };
                assert_eq!(ctx.builder.locals.len(), initial_locals + snapshots);
                // Each snapshot owns one callable root plus its parameter and result trees.
                assert_eq!(
                    REBUILT_TYPE_NODES.get(),
                    snapshots * (1 + 2 * (type_depth + 1))
                );
                assert_eq!(ctx.pending.len(), depth + 1);
                for (i, work) in ctx.pending[..depth].iter().enumerate() {
                    match work {
                        Work::DirectCall {
                            callee, signature, ..
                        } if !indirect => {
                            assert!(std::ptr::eq(*signature, &resolution.functions[callee]))
                        }
                        Work::IndirectCall { callee, .. } if indirect => {
                            assert_eq!(callee.0 as usize, initial_locals + i)
                        }
                        _ => panic!("expected pending call"),
                    }
                }

                while let Some(work) = ctx.pending.pop() {
                    ctx.step(work).unwrap();
                }
                let calls = ctx
                    .builder
                    .blocks
                    .iter()
                    .flat_map(|(insts, _)| insts)
                    .filter(|inst| match inst {
                        SourceInst::Compute {
                            value: LirRvalue::IndirectCall { .. },
                            ..
                        } => indirect,
                        SourceInst::Compute {
                            value: LirRvalue::DirectCall { .. },
                            ..
                        } => !indirect,
                        _ => false,
                    })
                    .count();
                assert_eq!(calls, depth);
            }
        }
    }
}

#[test]
fn interface_argument_validation_skips_the_declared_result() {
    for depth in [0, 8, 32] {
        let result = |leaf: &str| (0..depth).fold(leaf.to_owned(), |ty, _| format!("Array<{ty}>"));
        let hir = super::super::tests::checked_hir(&format!(
            "interface Source<T> {{ fn read(self) -> {}; }}
             fn probe(obj: Source<i64>, flag: bool) -> {} {{ return (flag ? obj : obj).read(); }}",
            result("T"),
            result("i64"),
        ));
        let (f, root) = returning_probe(&hir.functions);
        let resolution = LoweringDeclarations::new(&hir.resolution);
        let mut builder = Builder::new(&f.params, false, &resolution);
        let mut ctx = NestedCfg::new(&mut builder, root);
        REBUILT_TYPE_NODES.set(0);
        let work = ctx.pending.pop().unwrap();
        ctx.step(work).unwrap();
        assert!(
            matches!(&ctx.pending[..], [Work::PrepareMethod { conversions, .. }, Work::Eval(..)] if conversions.is_empty())
        );
        assert_eq!(REBUILT_TYPE_NODES.get(), 0, "result depth {depth}");

        let signature = method_signature(&resolution, &f.params[0].ty, "read").unwrap();
        assert_eq!(*signature.return_type(), f.return_type);
        assert!(REBUILT_TYPE_NODES.get() > depth);
    }
}

#[test]
fn queued_argument_coercions_use_the_validated_parameter() {
    for indirect in [false, true] {
        let local = if indirect {
            ", sink: fn(Base) -> i64"
        } else {
            ""
        };
        let hir = super::super::tests::checked_hir(&format!(
            "open class Base {{}} class Leaf extends Base {{}}
             fn sink(x: Base) -> i64 {{ return 1; }}
             fn probe(flag: bool, x: Leaf{local}) -> i64 {{ return sink(flag ? x : x); }}"
        ));
        let (f, root) = returning_probe(&hir.functions);
        let HirExprKind::Call { callee, args, .. } = &root.kind else {
            panic!("expected named call");
        };
        let resolution = LoweringDeclarations::new(&hir.resolution);
        let mut builder = Builder::new(&f.params, false, &resolution);
        let mut ctx = NestedCfg::new(&mut builder, root);
        // Compatibility checks can visit base classes. Charge that work separately.
        let params = &resolution.functions[callee].params;
        REBUILT_TYPE_NODES.set(0);
        assert!(arguments_compatible(&resolution, params, args));
        let validation = REBUILT_TYPE_NODES.get();
        REBUILT_TYPE_NODES.set(0);
        let work = ctx.pending.pop().unwrap();
        ctx.step(work).unwrap();
        assert_eq!(
            REBUILT_TYPE_NODES.get(),
            validation + if indirect { 3 } else { 0 }
        );
        match &ctx.pending[..] {
            [
                Work::IndirectCall { callee, .. },
                Work::CoerceSnapshotArgument(snapshot, 0),
                Work::Eval(..),
            ] => assert!(indirect && callee == snapshot),
            [
                Work::DirectCall { .. },
                Work::CoerceArgument(ParameterType::Declared(target)),
                Work::Eval(..),
            ] => assert!(!indirect && std::ptr::eq(*target, &params[0])),
            _ => panic!("expected callee, conversion, and operand"),
        }
        while let Some(work) = ctx.pending.pop() {
            ctx.step(work).unwrap();
        }
        assert!(ctx.builder.blocks.iter().flat_map(|(insts, _)| insts).any(|inst| {
            matches!(inst, SourceInst::Compute { value: LirRvalue::Coerce { target, .. }, .. } if target == &params[0])
        }));
    }
}

#[test]
fn wide_format_pushes_borrowed_segments_in_evaluation_order() {
    use crate::interpolate::{DECODED_LITERAL_BYTES, SPEC_PARSES};
    for width in [1, 8, 64] {
        for literal_first in [false, true] {
            let prefix = if literal_first { "α{{x}}" } else { "" };
            let template = format!("{prefix}{}", "{}!".repeat(width));
            let args = (0..width)
                .map(|i| format!("flag ? {i} : 0"))
                .collect::<Vec<_>>()
                .join(",");
            let hir = super::super::tests::checked_hir(&format!(
                "fn probe(flag: bool) -> String {{ return format(\"{template}\", {args}); }}"
            ));
            let f = hir
                .functions
                .iter()
                .find(|f| f.name.is_free_named("probe"))
                .unwrap();
            let HirStmt::Return {
                value: Some(root), ..
            } = &f.body[0]
            else {
                panic!("expected return")
            };
            let HirExprKind::Call { args, .. } = &root.kind else {
                panic!("expected format call")
            };
            let resolution = LoweringDeclarations::new(&hir.resolution);
            let mut builder = Builder::new(&f.params, false, &resolution);
            let mut ctx = NestedCfg::new(&mut builder, root);
            SPEC_PARSES.set(0);
            DECODED_LITERAL_BYTES.set(0);
            let enter = ctx.pending.pop().unwrap();
            ctx.step(enter).unwrap();
            assert_eq!(SPEC_PARSES.get(), 1);
            assert_eq!(DECODED_LITERAL_BYTES.get(), 0);
            assert_eq!(ctx.pending.len(), width * 3 + usize::from(literal_first));
            let mut actions = ctx.pending.iter().rev();
            if literal_first {
                assert!(matches!(
                    actions.next(),
                    Some(Work::FormatLiteral(_, "α{{x}}", true, _))
                ));
            }
            for (i, arg) in args[1..].iter().enumerate() {
                assert!(
                    matches!(actions.next(), Some(Work::Eval(child, false, false)) if std::ptr::eq(*child,arg))
                );
                assert!(
                    matches!(actions.next(), Some(Work::FormatValue(_, None, first, _)) if *first == (i==0 && !literal_first))
                );
                assert!(matches!(
                    actions.next(),
                    Some(Work::FormatLiteral(_, "!", false, _))
                ));
            }
            while let Some(work) = ctx.pending.pop() {
                ctx.step(work).unwrap();
            }
            assert_eq!(
                DECODED_LITERAL_BYTES.get(),
                width + if literal_first { "α{x}".len() } else { 0 }
            );
            assert_eq!(SPEC_PARSES.get(), 1);
        }
    }
}

// Count the pending recipes and check that their receiver arguments point
// into the original HIR. Neither variant can own a specialized type.
fn pending_interface_conversions(pending: &[Work<'_>]) -> usize {
    let mut count = 0;
    for work in pending {
        match work {
            Work::PrepareMethod {
                call, conversions, ..
            } => {
                for (_, target) in conversions {
                    if let ParameterType::Projected(p) = target {
                        let Type::Generic(_, args) = &call.object.ty else {
                            panic!("expected generic receiver")
                        };
                        assert!(std::ptr::eq(p.bindings.args.unwrap(), args.as_slice()));
                        count += 1;
                    }
                }
            }
            Work::CoerceArgument(ParameterType::Projected(_)) => count += 1,
            _ => {}
        }
    }
    count
}

fn receiver_chain(depth: usize, layers: usize, distinct: bool, convert: bool) -> String {
    let mut source = String::from("interface View<T> {} ");
    let mut params = Vec::new();
    let mut methods = String::new();
    let mut chain = String::new();
    let mut bindings = Vec::new();
    for i in 0..if distinct { depth } else { 1 } {
        let inner = (0..layers).fold(format!("Tag{i}"), |ty, _| format!("Array<{ty}>"));
        source.push_str(&format!(
            "class Tag{i} {{}} class Leaf{i} implements View<{inner}> {{}} "
        ));
        let arg = if convert {
            format!("Leaf{i}")
        } else {
            format!("View<{inner}>")
        };
        params.push(format!("x{i}: {arg}"));
        bindings.push(inner);
    }
    for i in 0..bindings.len() {
        let next = &bindings[(i + 1) % bindings.len()];
        methods.push_str(&format!("fn link{i}(self, x: View<T>) -> Chain<{next}>; "));
    }
    for i in 0..depth {
        let i = i % bindings.len();
        chain.push_str(&format!(".link{i}(x{i})"));
    }
    let first = &bindings[0];
    format!(
        "{source} interface Chain<T> {{ {methods} }}
             fn probe(first: Option<Chain<{first}>>, {}) -> Option<i64> {{
                 let result: Chain<{first}> = first?{chain}; return Option::Some(1);
             }}",
        params.join(",")
    )
}

#[test]
fn method_receiver_chains_materialize_conversions_only_at_emission() {
    for layers in [0, 8, 32] {
        for depth in [1, 8, 32] {
            for (distinct, convert) in [(false, false), (true, false), (false, true), (true, true)]
            {
                let source = receiver_chain(depth, layers, distinct, convert);
                let hir = super::super::tests::checked_hir(&source);
                let f = hir
                    .functions
                    .iter()
                    .find(|f| f.name.is_free_named("probe"))
                    .unwrap();
                let HirStmt::Let { value: root, .. } = &f.body[0] else {
                    panic!("expected receiver chain")
                };
                let resolution = LoweringDeclarations::new(&hir.resolution);
                let mut builder = Builder::new(&f.params, false, &resolution);
                builder.return_type = f.return_type.clone();
                let mut ctx = NestedCfg::new(&mut builder, root);
                DECLARED_METHOD_LOOKUPS.set(0);
                REBUILT_TYPE_NODES.set(0);
                INTERFACE_SPECIALIZATIONS.set(0);
                for _ in 0..depth {
                    let work = ctx.pending.pop().unwrap();
                    ctx.step(work).unwrap();
                }
                assert_eq!(DECLARED_METHOD_LOOKUPS.get(), depth);
                assert_eq!(REBUILT_TYPE_NODES.get(), 0);
                let expected = if convert { depth } else { 0 };
                assert_eq!(pending_interface_conversions(&ctx.pending), expected);
                assert_eq!(
                    ctx.pending
                        .iter()
                        .filter(|w| matches!(w, Work::PrepareMethod { .. }))
                        .count(),
                    depth
                );
                let mut emitted = 0;
                while let Some(work) = ctx.pending.pop() {
                    if matches!(work, Work::CoerceArgument(ParameterType::Projected(_))) {
                        emitted += 1;
                    }
                    ctx.step(work).unwrap();
                    assert_eq!(INTERFACE_SPECIALIZATIONS.get(), emitted);
                }
                assert_eq!(emitted, expected);
                let Some(SourceTerminator::Branch {
                    then_block,
                    else_block,
                    ..
                }) = &ctx.builder.blocks[0].1
                else {
                    panic!("receiver must propagate before any method")
                };
                for (block, success) in [(0, false), (then_block.0, true), (else_block.0, false)] {
                    let mut targets = Vec::new();
                    let mut calls = 0;
                    for inst in &ctx.builder.blocks[block].0 {
                        if let SourceInst::Compute { value, .. } = inst {
                            match value {
                                LirRvalue::MethodCall { .. } => calls += 1,
                                LirRvalue::Coerce { target, .. } => targets.push(target),
                                _ => {}
                            }
                        }
                    }
                    assert_eq!(calls, if success { depth } else { 0 });
                    assert_eq!(targets.len(), if success { expected } else { 0 });
                    for (i, target) in targets.into_iter().enumerate() {
                        let Type::Generic(view, args) = target else {
                            panic!("expected View<T>")
                        };
                        assert_eq!(*view, TypeId::local("View"));
                        let [binding] = args.as_slice() else {
                            panic!("expected one binding")
                        };
                        let mut inner = binding;
                        for _ in 0..layers {
                            let Type::Array(child) = inner else {
                                panic!("expected array")
                            };
                            inner = child;
                        }
                        assert_eq!(
                            *inner,
                            Type::Named(TypeId::local(format!(
                                "Tag{}",
                                if distinct { i } else { 0 }
                            )))
                        );
                    }
                }
                assert!(matches!(
                    ctx.builder.blocks[else_block.0].1,
                    Some(SourceTerminator::Return(_))
                ));
            }
        }
    }
}

#[test]
fn borrowed_interface_comparison_preserves_self_and_type_shape() {
    let class = TypeId::local("Service");
    let self_ty = Type::Named(TypeId::local("Self"));
    let t = TypeId::local("T");
    let concrete = Type::FixedArray(Box::new(Type::I64), 2);
    let params = [t].into_iter().collect();
    let empty = Default::default();
    for args in [Some(std::slice::from_ref(&concrete)), None] {
        let receiver = InterfaceReceiver {
            class,
            type_params: if args.is_some() { &params } else { &empty },
            args,
        };
        let bindings = receiver.bindings();
        for param in [
            self_ty.clone(),
            Type::Generic(class, vec![self_ty.clone(), Type::Named(t)]),
            Type::Fn(vec![Type::Named(t)], Box::new(self_ty.clone())),
            Type::Closure(vec![self_ty.clone()], Box::new(Type::Named(t))),
            Type::FixedArray(Box::new(Type::Named(t)), 2),
        ] {
            let target = bindings.specialize(&param);
            let mut alternatives = vec![
                target.clone(),
                Type::Void,
                Type::Named(class),
                Type::Generic(class, vec![]),
                Type::FixedArray(Box::new(concrete.clone()), 3),
            ];
            if let Type::Fn(args, ret) = &target {
                alternatives.push(Type::Closure(args.clone(), ret.clone()));
            }
            for ty in &alternatives {
                REBUILT_TYPE_NODES.set(0);
                assert_eq!(bindings.matches(&param, ty), target == *ty);
                assert_eq!(REBUILT_TYPE_NODES.get(), 0);
            }
        }
    }
}

#[test]
fn converted_self_parameters_borrow_the_whole_receiver() {
    for receiver_arity in [1, 8, 32] {
        let type_names = (0..receiver_arity)
            .map(|i| format!("T{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let class = TypeId::local("Service");
        let receiver = Type::Generic(class, vec![Type::I64; receiver_arity]);
        for count in [1, 8, 32] {
            let params = (0..count)
                .map(|i| format!("x{i}: Self"))
                .collect::<Vec<_>>()
                .join(",");
            let hir = super::super::tests::checked_hir(&format!(
                "interface Service<{type_names}> {{ fn take(self, {params}) -> i64; }}"
            ));
            let declarations = LoweringDeclarations::new(&hir.resolution);
            let sig = method_signature(&declarations, &receiver, "take").unwrap();
            let mut args = vec![
                HirExpr {
                    kind: HirExprKind::Var("exact".to_string()),
                    span: Span::dummy(),
                    ty: receiver.clone(),
                };
                count
            ];
            // Never is accepted by scalar argument validation for every value target.
            args[count - 1].ty = Type::Never;
            let mut conversions = Vec::new();
            REBUILT_TYPE_NODES.set(0);
            crate::parser::ast::HASHED_TYPE_NODES.set(0);
            assert!(sig.check_conversions(
                &args,
                |_| false,
                |index, target, _| {
                    conversions.push((index, target));
                    true
                }
            ));
            let [(index, ParameterType::Projected(p))] = conversions.as_slice() else {
                panic!("only the mismatched parameter requires conversion")
            };
            assert_eq!(*index, count - 1);
            assert!(std::ptr::eq(p.param, &sig.declaration.params[*index]));
            let Type::Generic(_, receiver_types) = &receiver else {
                unreachable!()
            };
            assert!(std::ptr::eq(
                p.bindings.args.unwrap(),
                receiver_types.as_slice()
            ));
            assert_eq!(REBUILT_TYPE_NODES.get(), 0);
            assert_eq!(crate::parser::ast::HASHED_TYPE_NODES.get(), 0);
            let mut builder = Builder::new(&[], false, &declarations);
            let ctx = NestedCfg::new(&mut builder, &args[0]);
            assert!(ctx.method_conversions(&sig, &args).is_none());
            assert_eq!(REBUILT_TYPE_NODES.get(), 0);
        }
    }
}

#[test]
fn sequential_and_overlapping_calls_keep_only_borrowed_conversions() {
    for width in [1, 8, 32] {
        let generic = (0..width)
            .map(|i| format!("T{i}"))
            .collect::<Vec<_>>()
            .join(",");
        for count in [1, 8, 32] {
            for overlap in [false, true] {
                let mut source = format!(
                    "interface View<{generic}> {{}}
                     interface Sink<{generic},Unused> {{ fn take(self, n: i64, x: View<{generic}>) -> i64; }}"
                );
                let mut params = vec!["flag: bool".to_string()];
                for i in 0..if overlap { 1 } else { count } {
                    let types = vec![format!("Tag{i}"); width].join(",");
                    source.push_str(&format!(
                        "class Tag{i} {{}} class Leaf{i} implements View<{types}> {{}} "
                    ));
                    params.push(format!("s{i}: Sink<{types},bool>, x{i}: Leaf{i}"));
                }
                let conditional = "flag ? 1 : 0";
                let calls = if overlap {
                    (0..count).fold(conditional.to_owned(), |inner, _| {
                        format!("s0.take({inner}, x0)")
                    })
                } else {
                    (0..count)
                        .map(|i| format!("s{i}.take({conditional}, x{i})"))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                source.push_str(&format!(
                    "fn probe({}) -> Array<i64> {{ return [{calls}]; }}",
                    params.join(",")
                ));
                let hir = super::super::tests::checked_hir(&source);
                let f = hir
                    .functions
                    .iter()
                    .find(|f| f.name.is_free_named("probe"))
                    .unwrap();
                let HirStmt::Return {
                    value: Some(root), ..
                } = &f.body[0]
                else {
                    panic!("expected return")
                };
                let resolution = LoweringDeclarations::new(&hir.resolution);
                let mut builder = Builder::new(&f.params, false, &resolution);
                let mut ctx = NestedCfg::new(&mut builder, root);
                crate::ir::typed_ast::BINDING_INDEX_ENTRIES.set(0);
                let mut peak_consumers = 0;
                let mut emitted = 0;
                INTERFACE_SPECIALIZATIONS.set(0);
                while let Some(work) = ctx.pending.pop() {
                    if matches!(work, Work::CoerceArgument(ParameterType::Projected(_))) {
                        emitted += 1;
                    }
                    ctx.step(work).unwrap();
                    assert_eq!(INTERFACE_SPECIALIZATIONS.get(), emitted);
                    peak_consumers =
                        peak_consumers.max(pending_interface_conversions(&ctx.pending));
                }
                assert_eq!(crate::ir::typed_ast::BINDING_INDEX_ENTRIES.get(), width + 1);
                assert_eq!(peak_consumers, if overlap { count } else { 1 });
                assert_eq!(emitted, count);
                let calls = ctx
                    .builder
                    .blocks
                    .iter()
                    .flat_map(|(insts, _)| insts)
                    .filter(|i| {
                        matches!(
                            i,
                            SourceInst::Compute {
                                value: LirRvalue::MethodCall { .. },
                                ..
                            }
                        )
                    })
                    .count();
                assert_eq!(calls, count);
            }
        }
    }
}

#[test]
fn borrowed_conversion_checks_callable_storage_and_nominal_ancestors() {
    use crate::ir::typed_ast::{HirClassInfo, HirInterfaceInfo, HirResolution};
    let t = TypeId::local("T");
    let self_id = TypeId::local("Self");
    let view = TypeId::local("View");
    let sink = TypeId::local("Sink");
    let parent = TypeId::local("Parent");
    let child = TypeId::local("Child");
    let implemented = TypeId::local("Implemented");
    let other = TypeId::local("Other");
    let params = [t].into_iter().collect();
    let exact = Type::Fn(vec![Type::Bool], Box::new(Type::I64));
    let closure = Type::Closure(vec![Type::Bool], Box::new(Type::I64));
    let receiver_args = [exact.clone()];
    let bindings = InterfaceReceiver {
        class: sink,
        type_params: &params,
        args: Some(&receiver_args),
    }
    .bindings();
    let view_of = |ty| Type::Generic(view, vec![ty]);
    let mut resolution = HirResolution::default();
    resolution.classes.insert(
        child,
        HirClassInfo {
            base: Some(parent),
            ..Default::default()
        },
    );
    resolution.classes.insert(
        parent,
        HirClassInfo {
            implements: vec![view_of(exact.clone()), Type::Named(implemented)],
            ..Default::default()
        },
    );
    resolution.interfaces.insert(
        implemented,
        HirInterfaceInfo {
            extends: vec![other],
            ..Default::default()
        },
    );
    // Lowering must also terminate if a malformed declaration graph contains a cycle.
    resolution.interfaces.insert(
        other,
        HirInterfaceInfo {
            extends: vec![implemented],
            ..Default::default()
        },
    );
    let cases = [
        (view_of(Type::Named(t)), Type::Named(child), true),
        (view_of(closure.clone()), Type::Named(child), false),
        (view_of(Type::Named(t)), view_of(closure.clone()), true),
        (
            Type::Named(self_id),
            Type::Generic(sink, vec![closure]),
            true,
        ),
        (Type::Named(other), Type::Named(child), true),
        (Type::Named(child), Type::Named(other), false),
        (
            Type::Array(Box::new(Type::Named(t))),
            Type::FixedArray(Box::new(exact), 2),
            false,
        ),
    ];
    for (param, source, expected) in cases {
        let concrete = bindings.specialize(&param);
        assert_eq!(resolution.can_coerce(&source, &concrete), expected);
        REBUILT_TYPE_NODES.set(0);
        assert_eq!(
            ProjectedParameter {
                param: &param,
                bindings
            }
            .accepts_value(&source, &resolution),
            expected
        );
        assert_eq!(REBUILT_TYPE_NODES.get(), 0);
    }
}

#[test]
fn construction_lookups_nested_dispatch_empty_and_missing_variants() {
    super::super::tests::check_empty_and_missing_variants(|resolution, expr| {
        let mut builder = Builder::new(&[], false, resolution);
        let mut ctx = NestedCfg::new(&mut builder, expr);
        ctx.schedule(expr).expect("static call dispatch")
    });
}

#[test]
fn inherited_method_cache_nested_misses() {
    super::super::tests::check_inherited_method_misses(|resolution, expr| {
        let mut builder = Builder::new(&[], false, resolution);
        NestedCfg::new(&mut builder, expr)
            .schedule(expr)
            .expect("method dispatch")
    });
}
