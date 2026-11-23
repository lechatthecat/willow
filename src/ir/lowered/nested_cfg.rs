use super::*;
use crate::ir::typed_ast::HirSignature;

/// Evaluate eager operands with an explicit worklist. The builder continues to
/// own emitted instructions, locals and names; this context only borrows it.
struct NestedCfg<'b, 'e, 'r> {
    builder: &'b mut Builder<'r>,
    resolution: &'r LoweringDeclarations<'r>,
    pending: Vec<Work<'e>>,
    values: Vec<HirExpr>,
    references: std::collections::HashMap<*const HirExpr, LirOperand>,
    control_flow: std::collections::HashSet<*const HirExpr>,
}

pub(super) fn lower(builder: &mut Builder, value: &HirExpr) -> Option<HirExpr> {
    if !(expression_needs_cfg(value) || builder.is_async && suspends_anywhere(value)) {
        return None;
    }
    let mut context = NestedCfg::new(builder, value);
    while let Some(work) = context.pending.pop() {
        context.step(work)?;
    }
    context.values.pop()
}

/// Ordinary argument results start here. Captured places live in `references`
/// until their own call consumes them, including across nested calls.
struct CallArguments<'a> {
    source: &'a [HirExpr],
    value_start: usize,
}

enum Work<'a> {
    Eval(&'a HirExpr, bool, bool), // expression, place, root
    Finish(&'a HirExpr, usize, bool),
    ArrayStore(LirLocalId, usize, &'a Type, Span),
    FieldStore(LirLocalId, &'a Type, &'a str, Span),
    EnumStore(LirLocalId, TypeId, &'a str, usize, &'a Type, Span),
    Constructor {
        object: LirLocalId,
        class: TypeId,
        args: CallArguments<'a>,
        span: &'a Span,
    },
    PrepareMethod {
        expr: &'a HirExpr,
        call: MethodOperands<'a>,
        callee: FunctionId,
        conversions: Vec<(usize, ParameterType<'a>)>,
    },
    MethodCall(&'a HirExpr, MethodOperands<'a>, LirLocalId, usize),
    IndirectCall {
        span: &'a Span,
        name: FunctionId,
        arity: usize,
        callee: LirLocalId,
    },
    DirectCall {
        span: &'a Span,
        callee: FunctionId,
        args: CallArguments<'a>,
        signature: &'a HirSignature,
    },
    StaticCall {
        expr: &'a HirExpr,
        class: TypeId,
        method: &'a str,
        args: CallArguments<'a>,
    },
    CaptureReference {
        expr: &'a HirExpr,
        place: &'a HirExpr,
        callee: FunctionId,
        index: usize,
    },
    FinishReference {
        expr: &'a HirExpr,
        span: &'a Span,
        captured: LirPlace,
        display: String,
    },
    CoerceArgument(ParameterType<'a>),
    CoerceSnapshotArgument(LirLocalId, usize),
    FormatLiteral(LirLocalId, &'a str, bool, Span),
    FormatValue(
        LirLocalId,
        Option<crate::interpolate::F64Format>,
        bool,
        Span,
    ),
}

fn callable_signature(ty: &Type) -> Option<(&[Type], &Type)> {
    match ty {
        Type::Fn(params, result) | Type::Closure(params, result) => Some((params, result)),
        _ => None,
    }
}

fn snapshot_signature(local: &LirLocal) -> (&[Type], &Type) {
    callable_signature(&local.ty).expect("an indirect call snapshot holds a function or closure")
}

fn push_arguments<'e>(
    pending: &mut Vec<Work<'e>>,
    callee: FunctionId,
    args: &'e [HirExpr],
    mut coercion: impl FnMut(usize, &HirExpr) -> Option<Work<'e>>,
) {
    for (index, arg) in args.iter().enumerate().rev() {
        if let HirExprKind::ReferenceArg { place } = &arg.kind {
            pending.push(Work::CaptureReference {
                expr: arg,
                place,
                callee,
                index,
            });
        } else {
            if let Some(work) = coercion(index, arg) {
                pending.push(work);
            }
            pending.push(Work::Eval(arg, false, false));
        }
    }
}

fn push_declared_arguments<'e>(
    pending: &mut Vec<Work<'e>>,
    callee: FunctionId,
    args: &'e [HirExpr],
    params: &'e [Type],
) {
    debug_assert_eq!(params.len(), args.len());
    push_arguments(pending, callee, args, |i, arg| {
        (params[i] != arg.ty).then_some(Work::CoerceArgument(ParameterType::Declared(&params[i])))
    });
}

fn push_method_arguments<'e>(
    pending: &mut Vec<Work<'e>>,
    callee: FunctionId,
    args: &'e [HirExpr],
    conversions: Vec<(usize, ParameterType<'e>)>,
) {
    let mut conversions = conversions.into_iter().rev().peekable();
    push_arguments(pending, callee, args, |index, _| {
        conversions
            .next_if(|(i, _)| *i == index)
            .map(|(_, target)| Work::CoerceArgument(target))
    });
}

impl<'b, 'e, 'r: 'e> NestedCfg<'b, 'e, 'r> {
    fn new(builder: &'b mut Builder<'r>, value: &'e HirExpr) -> Self {
        let mut control_flow = std::collections::HashSet::new();
        for node in value.walk_postorder(false) {
            if matches!(
                node.kind,
                HirExprKind::TryPropagate { .. }
                    | HirExprKind::Ternary { .. }
                    | HirExprKind::Match { .. }
                    | HirExprKind::Binary {
                        op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                        ..
                    }
            ) || (builder.is_async && expr_suspends_here(node))
                || (!matches!(node.kind, HirExprKind::Lambda { .. })
                    && node
                        .children()
                        .iter()
                        .any(|child| control_flow.contains(&std::ptr::from_ref(*child))))
            {
                control_flow.insert(std::ptr::from_ref(node));
            }
        }

        Self {
            resolution: builder.resolution,
            builder,
            control_flow,
            pending: vec![Work::Eval(value, false, true)],
            values: Vec::new(),
            references: std::collections::HashMap::new(),
        }
    }

    fn schedule_direct_call(
        &mut self,
        span: &'e Span,
        callee: FunctionId,
        args: &'e [HirExpr],
        signature: &'e HirSignature,
    ) -> Option<bool> {
        if args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.builder.push_compute(
                LirRvalue::BeginReferenceCall,
                Type::Void,
                *span,
                "reference_call",
            );
        }
        self.pending.push(Work::DirectCall {
            span,
            callee,
            args: CallArguments {
                source: args,
                value_start: self.values.len(),
            },
            signature,
        });
        push_declared_arguments(&mut self.pending, callee, args, &signature.params);
        Some(true)
    }

    fn method_conversions(
        &self,
        signature: &ResolvedSignature<'e, 'e>,
        args: &[HirExpr],
    ) -> Option<Vec<(usize, ParameterType<'e>)>> {
        let mut conversions = Vec::new();
        signature
            .check_conversions(
                args,
                |_| false,
                |index, target, arg| {
                    if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                        return false;
                    }
                    let compatible = match target {
                        ParameterType::Declared(ty) => self.resolution.can_coerce(&arg.ty, ty),
                        ParameterType::Projected(p) => p.accepts_value(&arg.ty, self.resolution),
                    };
                    if compatible {
                        conversions.push((index, target));
                    }
                    compatible
                },
            )
            .then_some(conversions)
    }

    fn schedule_static_call(
        &mut self,
        expr: &'e HirExpr,
        class: &TypeId,
        method: &'e str,
        args: &'e [HirExpr],
        conversions: Vec<(usize, ParameterType<'e>)>,
    ) -> Option<bool> {
        if args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.builder.push_compute(
                LirRvalue::BeginReferenceCall,
                Type::Void,
                expr.span,
                "reference_call",
            );
        }
        self.pending.push(Work::StaticCall {
            expr,
            class: *class,
            method,
            args: CallArguments {
                source: args,
                value_start: self.values.len(),
            },
        });
        push_method_arguments(
            &mut self.pending,
            FunctionId::method(*class, method),
            args,
            conversions,
        );
        Some(true)
    }

    fn schedule_format(
        &mut self,
        span: Span,
        args: &'e [HirExpr],
        template: &'e str,
        segments: &[crate::interpolate::SourceSegment],
    ) {
        let name = self.builder.synthetic_name("format_result");
        let local = self.builder.declare_local(
            name,
            Type::String,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.values.push(self.builder.local_expr(local, span));
        let mut children = args[1..].iter().rev();
        self.pending.reserve(segments.len().max(1) + args.len() - 1);
        if segments.is_empty() {
            self.pending
                .push(Work::FormatLiteral(local, "", true, span));
        }
        for (index, segment) in segments.iter().enumerate().rev() {
            match segment {
                crate::interpolate::SourceSegment::Literal(text) => self.pending.push(
                    Work::FormatLiteral(local, &template[text.clone()], index == 0, span),
                ),
                segment => {
                    let child = children.next().expect("format operand");
                    let format = match segment {
                        crate::interpolate::SourceSegment::F64(format) => Some(*format),
                        _ => None,
                    };
                    self.pending
                        .push(Work::FormatValue(local, format, index == 0, child.span));
                    self.pending.push(Work::Eval(child, false, false));
                }
            }
        }
    }

    fn schedule_indirect_call(
        &mut self,
        span: &'e Span,
        callee: FunctionId,
        args: &'e [HirExpr],
        source: LirLocalId,
    ) -> Option<bool> {
        let ty = &self.builder.locals[source.0 as usize].ty;
        let Some((params, _)) = callable_signature(ty) else {
            return Some(false);
        };
        if args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
            || !arguments_compatible(self.resolution, params, args)
        {
            return Some(false);
        }

        // Snapshot the target before argument side effects. Its local owns the signature.
        let snapshot_type = ty.clone();
        let name = self.builder.synthetic_name("callable_snapshot");
        let local = self.builder.declare_local(
            name,
            snapshot_type,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::Use(LirOperand::Local(source)),
            span: *span,
        });
        self.pending.push(Work::IndirectCall {
            span,
            name: callee,
            arity: args.len(),
            callee: local,
        });
        let (params, _) = snapshot_signature(&self.builder.locals[local.0 as usize]);
        push_arguments(&mut self.pending, callee, args, |index, arg| {
            (params[index] != arg.ty).then_some(Work::CoerceSnapshotArgument(local, index))
        });
        Some(true)
    }

    fn schedule_array(&mut self, expr: &'e HirExpr, elements: &'e [HirExpr]) -> Option<bool> {
        let Type::Array(element) = &expr.ty else {
            return None;
        };
        let name = self.builder.synthetic_name("array_shell");
        let local = self.builder.declare_local(
            name,
            expr.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::ArrayAlloc {
                length: elements.len(),
                element: (**element).clone(),
            },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));
        for (index, child) in elements.iter().enumerate().rev() {
            self.pending
                .push(Work::ArrayStore(local, index, element, child.span));
            self.pending.push(Work::Eval(child, false, false));
        }
        Some(true)
    }

    fn schedule_enum(
        &mut self,
        expr: &'e HirExpr,
        class: &TypeId,
        method: &'e str,
        args: &'e [HirExpr],
    ) -> Option<bool> {
        let name = self.builder.synthetic_name("enum_shell");
        let local = self.builder.declare_local(
            name,
            expr.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::EnumAlloc {
                class: *class,
                variant: method.to_owned(),
                enum_ty: expr.ty.clone(),
            },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));
        for (index, child) in args.iter().enumerate().rev() {
            self.pending.push(Work::EnumStore(
                local, *class, method, index, &expr.ty, child.span,
            ));
            self.pending.push(Work::Eval(child, false, false));
        }
        Some(true)
    }

    fn schedule_object_literal(
        &mut self,
        expr: &'e HirExpr,
        class: &TypeId,
        fields: &'e [(String, HirExpr)],
    ) -> Option<bool> {
        let name = self.builder.synthetic_name("object_shell");
        let local = self.builder.declare_local(
            name,
            expr.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::ObjectAlloc { class: *class },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));
        for (field, child) in fields.iter().rev() {
            self.pending
                .push(Work::FieldStore(local, &expr.ty, field, child.span));
            self.pending.push(Work::Eval(child, false, false));
        }
        Some(true)
    }

    fn schedule_new(
        &mut self,
        expr: &'e HirExpr,
        class: &TypeId,
        args: &'e [HirExpr],
    ) -> Option<bool> {
        let constructor = self.resolution.classes[class].constructor.as_ref();
        if constructor.is_some_and(|signature| {
            !arguments_compatible(self.resolution, &signature.params, args)
        }) {
            return None;
        }
        let fields = if constructor.is_none() {
            let fields = self.resolution.fields_rev(class)?;
            if fields.len() != args.len() {
                return None;
            }
            Some(fields)
        } else {
            None
        };
        let name = self.builder.synthetic_name("object_shell");
        let local = self.builder.declare_local(
            name,
            expr.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::ObjectAlloc { class: *class },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));
        if let Some(signature) = constructor {
            if args
                .iter()
                .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
            {
                self.builder.push_compute(
                    LirRvalue::BeginReferenceCall,
                    Type::Void,
                    expr.span,
                    "reference_call",
                );
            }
            self.pending.push(Work::Constructor {
                object: local,
                class: *class,
                args: CallArguments {
                    source: args,
                    value_start: self.values.len(),
                },
                span: &expr.span,
            });
            push_declared_arguments(
                &mut self.pending,
                FunctionId::method(*class, "init"),
                args,
                &signature.params,
            );
        } else if let Some(fields) = fields {
            for ((field, _), child) in fields.zip(args.iter().rev()) {
                self.pending
                    .push(Work::FieldStore(local, &expr.ty, field, child.span));
                self.pending.push(Work::Eval(child, false, false));
            }
        }
        Some(true)
    }

    // Some(true) queues a specialized path; Some(false) uses eager fallback.
    // None preserves a failed lowering attempt without undoing its prefix.
    fn schedule(&mut self, expr: &'e HirExpr) -> Option<bool> {
        match &expr.kind {
            HirExprKind::Call {
                callee,
                binding,
                args,
            } => {
                if binding.is_local() {
                    let source = *self.builder.local_by_name.get(callee.name().as_ref())?;
                    self.schedule_indirect_call(&expr.span, *callee, args, source)
                } else if let Some(signature) = self.resolution.functions.get(callee)
                    && !signature.is_async
                    && arguments_compatible(self.resolution, &signature.params, args)
                {
                    self.schedule_direct_call(&expr.span, *callee, args, signature)
                } else if callee.is_free_named("format") {
                    Some(with_format_segments(args, |template, segments| {
                        self.schedule_format(expr.span, args, template, segments)
                    }))
                } else {
                    Some(false)
                }
            }
            HirExprKind::MethodCall {
                object,
                method,
                args,
                builtin: None,
                super_class,
            } => {
                let (Type::Named(owner) | Type::Generic(owner, _)) = &object.ty else {
                    return Some(false);
                };
                let Some(signature) = method_signature(self.resolution, &object.ty, method) else {
                    return Some(false);
                };
                if signature.declaration.is_static
                    && !self.resolution.interfaces.contains_key(owner)
                {
                    return Some(false);
                }
                let Some(conversions) = self.method_conversions(&signature, args) else {
                    return Some(false);
                };
                self.pending.push(Work::PrepareMethod {
                    expr,
                    call: MethodOperands {
                        object,
                        method,
                        args,
                        super_class: *super_class,
                    },
                    callee: FunctionId::method(super_class.unwrap_or(*owner), method),
                    conversions,
                });
                self.pending.push(Work::Eval(object, false, false));
                Some(true)
            }
            HirExprKind::Array { elements } => self.schedule_array(expr, elements),
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } => {
                if let Some(signature) = static_signature(self.resolution, class, method)
                    && signature.declaration.is_static
                    && let Some(conversions) = self.method_conversions(&signature, args)
                {
                    return self.schedule_static_call(expr, class, method, args, conversions);
                }
                if self
                    .resolution
                    .enum_variant(class, method, Some(args.len()))
                    .is_some()
                {
                    self.schedule_enum(expr, class, method, args)
                } else {
                    Some(false)
                }
            }
            HirExprKind::ObjectLiteral { class, fields }
                if object_literal_supported(self.resolution, class, fields) =>
            {
                self.schedule_object_literal(expr, class, fields)
            }
            HirExprKind::New { class, args } if self.resolution.classes.contains_key(class) => {
                self.schedule_new(expr, class, args)
            }
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Invoke, Print, Index,
                Ternary, New, FieldAccess, MethodCall, ObjectLiteral, StaticField,
                ReferenceArg, Range, Await, TryPropagate, Lambda, Match, Select
            ) => Some(false),
        }
    }

    fn eval(&mut self, expr: &'e HirExpr, place: bool, root: bool) -> Option<()> {
        if !place && self.control_flow.contains(&std::ptr::from_ref(expr)) && self.schedule(expr)? {
            return Some(());
        }
        if matches!(
            expr.kind,
            HirExprKind::TryPropagate { .. }
                | HirExprKind::Ternary { .. }
                | HirExprKind::Match { .. }
                | HirExprKind::Binary {
                    op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                    ..
                }
        ) || (self.builder.is_async && expr_suspends_here(expr))
        {
            let name = self.builder.synthetic_name("expression");
            let result = self.builder.declare_local(
                name,
                expr.ty.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.builder.lower_root_suspend(expr, Some(result))?;
            self.values.push(self.builder.local_expr(result, expr.span));
        } else if matches!(
            expr.kind,
            HirExprKind::Lambda { .. } | HirExprKind::Select { .. }
        ) || !(self.control_flow.contains(&std::ptr::from_ref(expr))
            || matches!(expr.kind, HirExprKind::ReferenceArg { .. })
            || (place
                && matches!(
                    expr.kind,
                    HirExprKind::Index { .. } | HirExprKind::FieldAccess { .. }
                )))
        {
            if root
                || place
                || matches!(
                    expr.kind,
                    HirExprKind::Int(_)
                        | HirExprKind::Float(_)
                        | HirExprKind::Bool(_)
                        | HirExprKind::Str(_)
                        | HirExprKind::FnRef(_)
                        | HirExprKind::ReferenceArg { .. }
                )
            {
                self.values.push(expr.clone());
            } else {
                let name = self.builder.synthetic_name("expression_operand");
                let local = self.builder.declare_local(
                    name,
                    expr.ty.clone(),
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                );
                self.builder.lower_value_into(local, expr);
                self.values.push(self.builder.local_expr(local, expr.span));
            }
        } else {
            let operands = expr.children();
            let reference = matches!(expr.kind, HirExprKind::ReferenceArg { .. });
            self.pending.push(Work::Finish(
                expr,
                operands.len(),
                root || place || reference,
            ));
            self.pending.extend(
                operands
                    .into_iter()
                    .rev()
                    .map(|operand| Work::Eval(operand, reference, false)),
            );
        }

        Some(())
    }

    fn coerce_argument(&mut self, target: Type) -> Option<()> {
        let source = self.values.pop().expect("argument value");
        let value = self.builder.flat_operand(&source);
        let local = self.builder.push_compute(
            LirRvalue::Coerce {
                value,
                source: source.ty.clone(),
                target: target.clone(),
            },
            target,
            source.span,
            "coerced_argument",
        );
        self.values
            .push(self.builder.local_expr(local, source.span));

        Some(())
    }

    fn capture_reference(
        &mut self,
        expr: &'e HirExpr,
        place: &'e HirExpr,
        callee: FunctionId,
        index: usize,
    ) -> Option<()> {
        let (captured, children) = match &place.kind {
            HirExprKind::Var(name) => (
                LirPlace::Local(*self.builder.local_by_name.get(name)?),
                [None, None],
            ),
            HirExprKind::FieldAccess { object, field } => {
                let name = self.builder.synthetic_name("reference_object");
                (
                    LirPlace::Field {
                        object: self.builder.declare_local(
                            name,
                            object.ty.clone(),
                            None,
                            LocalOptions {
                                synthetic: true,
                                parameter: false,
                            },
                        ),
                        object_ty: object.ty.clone(),
                        field: field.clone(),
                        ty: place.ty.clone(),
                    },
                    [Some(object.as_ref()), None],
                )
            }
            HirExprKind::Index { array, index } => {
                let name = self.builder.synthetic_name("reference_owner");
                let owner = self.builder.declare_local(
                    name,
                    Type::Void,
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                );
                self.builder.locals[owner.0 as usize].storage_kind = LirStorageKind::GcOwner;
                let name = self.builder.synthetic_name("reference_index");
                (
                    LirPlace::ArrayElement {
                        owner,
                        index: self.builder.declare_local(
                            name,
                            Type::I64,
                            None,
                            LocalOptions {
                                synthetic: true,
                                parameter: false,
                            },
                        ),
                        element: place.ty.clone(),
                    },
                    [Some(array.as_ref()), Some(index.as_ref())],
                )
            }
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, FnRef, Binary, Unary, Call, Invoke, Print, Array, Ternary,
                New, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg, Range,
                Await, TryPropagate, Lambda, Match, Select
            ) => return None,
        };
        let display = reference_place_name(place);
        self.builder.push_compute(
            LirRvalue::ReferenceDebug {
                argument: LirOperand::Reference {
                    place: captured.clone(),
                    span: expr.span,
                    display: display.clone(),
                },
                callee,
                index,
            },
            Type::Void,
            expr.span,
            "reference_debug",
        );
        self.pending.push(Work::FinishReference {
            expr,
            span: &place.span,
            captured,
            display,
        });
        self.pending.extend(
            children
                .into_iter()
                .rev()
                .flatten()
                .map(|child| Work::Eval(child, false, false)),
        );
        Some(())
    }

    fn finish_reference(
        &mut self,
        expr: &'e HirExpr,
        span: Span,
        captured: LirPlace,
        display: String,
    ) -> Option<()> {
        match &captured {
            LirPlace::Local(_) => {}
            LirPlace::Field { object, .. } => {
                let child = self.values.pop().expect("reference receiver");
                let value = self.builder.flat_operand(&child);
                self.builder.push(SourceInst::Compute {
                    local: *object,
                    value: LirRvalue::Use(value),
                    span,
                });
            }
            LirPlace::ArrayElement { owner, index, .. } => {
                let index_expr = self.values.pop().expect("reference index");
                let array_expr = self.values.pop().expect("reference array");
                let index_value = self.builder.flat_operand(&index_expr);
                let array_value = self.builder.flat_operand(&array_expr);
                self.builder.push(SourceInst::Compute {
                    local: *index,
                    value: LirRvalue::Use(index_value.clone()),
                    span,
                });
                self.builder.push(SourceInst::Compute {
                    local: *owner,
                    value: LirRvalue::CaptureArrayOwner {
                        array: array_value,
                        index: LirOperand::Local(*index),
                    },
                    span,
                });
            }
        }
        let argument = LirOperand::Reference {
            place: captured,
            span: expr.span,
            display,
        };
        self.references.insert(std::ptr::from_ref(expr), argument);
        Some(())
    }

    fn take_call_arguments(
        &mut self,
        args: CallArguments<'_>,
        mut record_type: impl FnMut(&Type),
    ) -> Vec<LirOperand> {
        let mut children = self.values.drain(args.value_start..);
        args.source
            .iter()
            .map(|source| {
                if let Some(reference) = self.references.remove(&std::ptr::from_ref(source)) {
                    record_type(&source.ty);
                    reference
                } else {
                    let child = children
                        .next()
                        .expect("one evaluated value per ordinary argument");
                    record_type(&child.ty);
                    self.builder.flat_operand(&child)
                }
            })
            .collect()
    }

    fn direct_call(
        &mut self,
        span: Span,
        callee: FunctionId,
        args: CallArguments<'_>,
        signature: &HirSignature,
    ) -> Option<()> {
        let operands = self.take_call_arguments(args, |_| {});
        let local = self.builder.push_compute(
            LirRvalue::DirectCall {
                callee,
                args: operands,
                params: signature.params.clone(),
                result: signature.return_type.clone(),
            },
            signature.return_type.clone(),
            span,
            "call_result",
        );
        self.values.push(self.builder.local_expr(local, span));

        Some(())
    }

    fn static_call(
        &mut self,
        expr: &HirExpr,
        class: TypeId,
        method: &str,
        args: CallArguments<'_>,
    ) -> Option<()> {
        let mut arg_types = Vec::with_capacity(args.source.len());
        let operands = self.take_call_arguments(args, |ty| arg_types.push(ty.clone()));
        let local = self.builder.push_compute(
            LirRvalue::StaticCall {
                class,
                method: method.to_owned(),
                is_async: false,
                args: operands,
                arg_types,
                result: expr.ty.clone(),
            },
            expr.ty.clone(),
            expr.span,
            "static_result",
        );
        self.values.push(self.builder.local_expr(local, expr.span));

        Some(())
    }

    fn format_literal(
        &mut self,
        local: LirLocalId,
        text: String,
        first: bool,
        span: Span,
    ) -> Option<()> {
        let name = self.builder.synthetic_name("format_literal");
        let piece = self.builder.declare_local(
            name,
            Type::String,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local: piece,
            value: LirRvalue::StringLiteral(text),
            span,
        });
        self.builder
            .append_format_piece(local, LirOperand::Local(piece), first, span);

        Some(())
    }

    fn format_value(
        &mut self,
        local: LirLocalId,
        format: Option<crate::interpolate::F64Format>,
        first: bool,
        span: Span,
    ) -> Option<()> {
        let child = self.values.pop().expect("format operand");
        let value = self.builder.flat_operand(&child);
        let name = self.builder.synthetic_name("format_piece");
        let piece = self.builder.declare_local(
            name,
            Type::String,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local: piece,
            value: LirRvalue::FormatScalar {
                value,
                ty: child.ty.clone(),
                format,
            },
            span,
        });
        self.builder
            .append_format_piece(local, LirOperand::Local(piece), first, span);

        Some(())
    }

    fn indirect_call(
        &mut self,
        span: Span,
        name: FunctionId,
        arity: usize,
        callee: LirLocalId,
    ) -> Option<()> {
        let children = self.values.split_off(self.values.len() - arity);
        let args = children
            .iter()
            .map(|child| self.builder.flat_operand(child))
            .collect();
        let (params, result) = snapshot_signature(&self.builder.locals[callee.0 as usize]);
        let params = params.to_vec();
        let result = result.clone();
        let local_name = self.builder.synthetic_name("indirect_result");
        let local = self.builder.declare_local(
            local_name,
            result.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::IndirectCall {
                callee: LirOperand::Local(callee),
                name,
                args,
                params,
                result,
            },
            span,
        });
        self.values.push(self.builder.local_expr(local, span));

        Some(())
    }

    fn prepare_method(
        &mut self,
        expr: &'e HirExpr,
        call: MethodOperands<'e>,
        callee: FunctionId,
        conversions: Vec<(usize, ParameterType<'e>)>,
    ) -> Option<()> {
        let receiver = self.values.pop().expect("method receiver");
        let receiver = self.builder.flat_operand(&receiver);
        let name = self.builder.synthetic_name("method_receiver");
        let local = self.builder.declare_local(
            name,
            call.object.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::PrepareMethod {
                receiver,
                receiver_ty: call.object.ty.clone(),
                method: call.method.to_string(),
            },
            span: expr.span,
        });
        if call
            .args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.builder.push_compute(
                LirRvalue::BeginReferenceCall,
                Type::Void,
                expr.span,
                "reference_call",
            );
        }
        self.pending
            .push(Work::MethodCall(expr, call, local, self.values.len()));
        push_method_arguments(&mut self.pending, callee, call.args, conversions);

        Some(())
    }

    fn method_call(
        &mut self,
        expr: &HirExpr,
        call: MethodOperands<'_>,
        receiver: LirLocalId,
        value_start: usize,
    ) -> Option<()> {
        let mut arg_types = Vec::with_capacity(call.args.len());
        let operands = self.take_call_arguments(
            CallArguments {
                source: call.args,
                value_start,
            },
            |ty| arg_types.push(ty.clone()),
        );
        let name = self.builder.synthetic_name("method_result");
        let local = self.builder.declare_local(
            name,
            expr.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::MethodCall {
                receiver: LirOperand::Local(receiver),
                receiver_ty: call.object.ty.clone(),
                method: call.method.to_string(),
                // Filled once all method calls have been lowered.
                is_async: false,
                args: operands,
                arg_types,
                result: expr.ty.clone(),
                super_class: call.super_class,
            },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));

        Some(())
    }

    fn array_store(
        &mut self,
        array: LirLocalId,
        index: usize,
        element: &Type,
        span: Span,
    ) -> Option<()> {
        let child = self.values.pop().expect("array element");
        let value = self.builder.flat_operand(&child);
        let name = self.builder.synthetic_name("array_store");
        let local = self.builder.declare_local(
            name,
            Type::Void,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::ArrayStore {
                array: LirOperand::Local(array),
                index: LirOperand::Int(index as i64),
                value,
                element: element.clone(),
            },
            span,
        });

        Some(())
    }

    fn field_store(
        &mut self,
        object: LirLocalId,
        object_ty: &Type,
        field: &str,
        span: Span,
    ) -> Option<()> {
        let child = self.values.pop().expect("field value");
        let value = self.builder.flat_operand(&child);
        let name = self.builder.synthetic_name("field_store");
        let local = self.builder.declare_local(
            name,
            Type::Void,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::FieldStore {
                object: LirOperand::Local(object),
                object_ty: object_ty.clone(),
                field: field.to_owned(),
                value,
            },
            span,
        });

        Some(())
    }

    fn enum_store(
        &mut self,
        local: LirLocalId,
        class: TypeId,
        variant: &str,
        index: usize,
        enum_ty: &Type,
        span: Span,
    ) -> Option<()> {
        let child = self.values.pop().expect("enum payload");
        let value = self.builder.flat_operand(&child);
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::EnumPayloadStore {
                object: LirOperand::Local(local),
                class,
                variant: variant.to_owned(),
                index,
                value,
                source: child.ty.clone(),
                enum_ty: enum_ty.clone(),
            },
            span,
        });

        Some(())
    }

    fn constructor(
        &mut self,
        object: LirLocalId,
        class: TypeId,
        args: CallArguments<'_>,
        span: Span,
    ) -> Option<()> {
        let mut arg_types = Vec::with_capacity(args.source.len());
        let operands = self.take_call_arguments(args, |ty| arg_types.push(ty.clone()));
        self.builder.push_compute(
            LirRvalue::ConstructorCall {
                object: LirOperand::Local(object),
                class,
                args: operands,
                arg_types,
            },
            Type::Void,
            span,
            "constructor_call",
        );

        Some(())
    }

    fn finish(&mut self, expr: &'e HirExpr, count: usize, keep_expr: bool) -> Option<()> {
        let operands = self.values.split_off(self.values.len() - count);
        let lowered = expr.with_eager_operands(operands);
        if keep_expr {
            self.values.push(lowered);
        } else {
            let name = self.builder.synthetic_name("expression_operand");
            let local = self.builder.declare_local(
                name,
                lowered.ty.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.builder.lower_value_into(local, &lowered);
            self.values.push(self.builder.local_expr(local, expr.span));
        }

        Some(())
    }

    fn step(&mut self, work: Work<'e>) -> Option<()> {
        match work {
            Work::Eval(expr, place, root) => self.eval(expr, place, root),
            Work::CoerceArgument(target) => self.coerce_argument(match target {
                ParameterType::Declared(ty) => ty.clone(),
                ParameterType::Projected(p) => p.bindings.specialize(p.param),
            }),
            Work::CoerceSnapshotArgument(local, index) => {
                let (params, _) = snapshot_signature(&self.builder.locals[local.0 as usize]);
                self.coerce_argument(params[index].clone())
            }
            Work::CaptureReference {
                expr,
                place,
                callee,
                index,
            } => self.capture_reference(expr, place, callee, index),
            Work::FinishReference {
                expr,
                span,
                captured,
                display,
            } => self.finish_reference(expr, *span, captured, display),
            Work::DirectCall {
                span,
                callee,
                args,
                signature,
            } => self.direct_call(*span, callee, args, signature),
            Work::StaticCall {
                expr,
                class,
                method,
                args,
            } => self.static_call(expr, class, method, args),
            Work::FormatLiteral(local, text, first, span) => {
                self.format_literal(local, crate::interpolate::decode_literal(text), first, span)
            }
            Work::FormatValue(local, format, first, span) => {
                self.format_value(local, format, first, span)
            }
            Work::IndirectCall {
                span,
                name,
                arity,
                callee,
            } => self.indirect_call(*span, name, arity, callee),
            Work::PrepareMethod {
                expr,
                call,
                callee,
                conversions,
            } => self.prepare_method(expr, call, callee, conversions),
            Work::MethodCall(expr, call, receiver, start) => {
                self.method_call(expr, call, receiver, start)
            }
            Work::ArrayStore(array, index, element, span) => {
                self.array_store(array, index, element, span)
            }
            Work::FieldStore(object, object_ty, field, span) => {
                self.field_store(object, object_ty, field, span)
            }
            Work::EnumStore(local, class, variant, index, enum_ty, span) => {
                self.enum_store(local, class, variant, index, enum_ty, span)
            }
            Work::Constructor {
                object,
                class,
                args,
                span,
            } => self.constructor(object, class, args, *span),
            Work::Finish(expr, count, keep_expr) => self.finish(expr, count, keep_expr),
        }
    }
}

#[cfg(test)]
mod tests;
