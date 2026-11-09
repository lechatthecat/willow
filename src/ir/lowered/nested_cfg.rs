use super::*;

/// Evaluate eager operands with an explicit worklist. The builder continues to
/// own emitted instructions, locals and names; this context only borrows it.
struct NestedCfg<'b, 'e> {
    builder: &'b mut Builder,
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

enum Work<'a> {
    Eval(&'a HirExpr, bool, bool), // expression, place, root
    Finish(&'a HirExpr, usize, bool, bool),
    ArrayStore(LirLocalId, usize, Type, Span),
    FieldStore(LirLocalId, Type, String, Span),
    EnumStore(LirLocalId, TypeId, String, usize, Type, Span),
    Constructor(LirLocalId, &'a HirExpr),
    PrepareMethod(&'a HirExpr),
    MethodCall(&'a HirExpr, LirLocalId),
    IndirectCall(&'a HirExpr, LirLocalId, Vec<Type>, Type),
    DirectCall(&'a HirExpr, FunctionId, Vec<Type>, Type),
    StaticCall(&'a HirExpr),
    CaptureReference(&'a HirExpr, FunctionId, usize),
    FinishReference(&'a HirExpr, LirOperand),
    CoerceArgument(Type),
    FormatLiteral(LirLocalId, String, bool, Span),
    FormatValue(
        LirLocalId,
        Option<crate::interpolate::F64Format>,
        bool,
        Span,
    ),
}

fn arguments<'a>(callee: FunctionId, args: &'a [HirExpr], params: &[Type]) -> Vec<Work<'a>> {
    let mut pending = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
            pending.push(Work::CaptureReference(arg, callee, index));
        } else {
            pending.push(Work::Eval(arg, false, false));
            if let Some(target) = params.get(index)
                && *target != arg.ty
            {
                pending.push(Work::CoerceArgument(target.clone()));
            }
        }
    }
    pending
}

impl<'b, 'e> NestedCfg<'b, 'e> {
    fn new(builder: &'b mut Builder, value: &'e HirExpr) -> Self {
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
            builder,
            control_flow,
            pending: vec![Work::Eval(value, false, true)],
            values: Vec::new(),
            references: std::collections::HashMap::new(),
        }
    }

    fn schedule_direct_call(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::Call { callee, args } = &expr.kind else {
            unreachable!()
        };

        let signature = self.builder.resolution.functions[callee].clone();
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
        self.pending.push(Work::DirectCall(
            expr,
            *callee,
            signature.params.clone(),
            signature.return_type,
        ));
        self.pending.extend(
            arguments(*callee, args, &signature.params)
                .into_iter()
                .rev(),
        );
        Some(true)
    }

    fn schedule_static_call(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::StaticCall {
            class,
            method,
            args,
        } = &expr.kind
        else {
            unreachable!()
        };

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
        let signature =
            static_signature(&self.builder.resolution, class, method).expect("static signature");
        self.pending.push(Work::StaticCall(expr));
        self.pending.extend(
            arguments(FunctionId::method(*class, method), args, &signature.params)
                .into_iter()
                .rev(),
        );
        Some(true)
    }

    fn schedule_format(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::Call { args, .. } = &expr.kind else {
            unreachable!()
        };

        let mut segments = format_segments(args).expect("validated format");
        if segments.is_empty() {
            segments.push(crate::interpolate::Segment::Literal(String::new()));
        }
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
        self.values.push(self.builder.local_expr(local, expr.span));
        let mut children = args[1..].iter();
        let mut actions = Vec::new();
        for (index, segment) in segments.into_iter().enumerate() {
            match segment {
                crate::interpolate::Segment::Literal(text) => {
                    actions.push(Work::FormatLiteral(local, text, index == 0, expr.span))
                }
                segment => {
                    let child = children.next().expect("format operand");
                    let format = match segment {
                        crate::interpolate::Segment::F64(format) => Some(format),
                        _ => None,
                    };
                    actions.push(Work::Eval(child, false, false));
                    actions.push(Work::FormatValue(local, format, index == 0, child.span));
                }
            }
        }
        self.pending.extend(actions.into_iter().rev());
        Some(true)
    }

    fn schedule_indirect_call(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::Call { callee, args } = &expr.kind else {
            unreachable!()
        };

        let source = self.builder.local_by_name[callee.unqualified_name().as_deref().unwrap_or("")];
        let ty = self.builder.locals[source.0 as usize].ty.clone();
        if let Type::Fn(params, result) | Type::Closure(params, result) = &ty
            && !args
                .iter()
                .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
            && arguments_compatible(&self.builder.resolution, params, args)
        {
            let name = self.builder.synthetic_name("callable_snapshot");
            let local = self.builder.declare_local(
                name,
                ty.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.builder.push(SourceInst::Compute {
                local,
                value: LirRvalue::Use(LirOperand::Local(source)),
                span: expr.span,
            });
            self.pending.push(Work::IndirectCall(
                expr,
                local,
                params.clone(),
                (**result).clone(),
            ));
            self.pending
                .extend(arguments(*callee, args, params).into_iter().rev());
            return Some(true);
        }

        Some(false)
    }

    fn schedule_method_call(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::MethodCall { object, .. } = &expr.kind else {
            unreachable!()
        };

        self.pending.push(Work::PrepareMethod(expr));
        self.pending.push(Work::Eval(object, false, false));
        Some(true)
    }

    fn schedule_array(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::Array { elements } = &expr.kind else {
            unreachable!()
        };

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
            self.pending.push(Work::ArrayStore(
                local,
                index,
                (**element).clone(),
                child.span,
            ));
            self.pending.push(Work::Eval(child, false, false));
        }
        Some(true)
    }

    fn schedule_enum(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::StaticCall {
            class,
            method,
            args,
        } = &expr.kind
        else {
            unreachable!()
        };

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
                variant: method.clone(),
                enum_ty: expr.ty.clone(),
            },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));
        for (index, child) in args.iter().enumerate().rev() {
            self.pending.push(Work::EnumStore(
                local,
                *class,
                method.clone(),
                index,
                expr.ty.clone(),
                child.span,
            ));
            self.pending.push(Work::Eval(child, false, false));
        }
        Some(true)
    }

    fn schedule_object_literal(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::ObjectLiteral { class, fields } = &expr.kind else {
            unreachable!()
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
        for (field, child) in fields.iter().rev() {
            self.pending.push(Work::FieldStore(
                local,
                expr.ty.clone(),
                field.clone(),
                child.span,
            ));
            self.pending.push(Work::Eval(child, false, false));
        }
        Some(true)
    }

    fn schedule_new(&mut self, expr: &'e HirExpr) -> Option<bool> {
        let HirExprKind::New { class, args } = &expr.kind else {
            unreachable!()
        };

        let info = self.builder.resolution.classes[class].clone();
        if info.constructor.as_ref().is_some_and(|signature| {
            !arguments_compatible(&self.builder.resolution, &signature.params, args)
        }) {
            return None;
        }
        let fields = if info.constructor.is_none() {
            class_fields(&self.builder.resolution, class)
        } else {
            Some(Vec::new())
        }?;
        if info.constructor.is_none() && fields.len() != args.len() {
            return None;
        }
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
        if let Some(constructor) = &info.constructor {
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
            self.pending.push(Work::Constructor(local, expr));
            self.pending.extend(
                arguments(
                    FunctionId::method(*class, "init"),
                    args,
                    &constructor.params,
                )
                .into_iter()
                .rev(),
            );
        } else {
            for ((field, _), child) in fields.into_iter().zip(args).rev() {
                self.pending
                    .push(Work::FieldStore(local, expr.ty.clone(), field, child.span));
                self.pending.push(Work::Eval(child, false, false));
            }
        }
        Some(true)
    }

    // Some(true) queues a specialized path; Some(false) uses eager fallback.
    // None preserves a failed lowering attempt without undoing its prefix.
    fn schedule(&mut self, expr: &'e HirExpr) -> Option<bool> {
        match &expr.kind {
            HirExprKind::Call { callee, args } if self.direct_call_supported(callee, args) => {
                self.schedule_direct_call(expr)
            }
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } if self.static_call_supported(class, method, args) => self.schedule_static_call(expr),
            HirExprKind::Call { callee, args }
                if callee.is_free_named("format")
                    && !self.builder.local_by_name.contains_key("format")
                    && format_segments(args).is_some() =>
            {
                self.schedule_format(expr)
            }
            HirExprKind::Call { callee, .. } if self.is_local_callable(callee) => {
                self.schedule_indirect_call(expr)
            }
            HirExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } if self.method_call_supported(object, method, args) => {
                self.schedule_method_call(expr)
            }
            HirExprKind::Array { .. } => self.schedule_array(expr),
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } if self
                .builder
                .resolution
                .enums
                .get(class)
                .is_some_and(|info| {
                    info.variants.iter().any(|variant| {
                        variant.name == *method && variant.payloads.len() == args.len()
                    })
                }) =>
            {
                self.schedule_enum(expr)
            }
            HirExprKind::ObjectLiteral { class, fields }
                if self.object_literal_supported(class, fields) =>
            {
                self.schedule_object_literal(expr)
            }
            HirExprKind::New { class, .. }
                if self.builder.resolution.classes.contains_key(class) =>
            {
                self.schedule_new(expr)
            }
            _ => Some(false),
        }
    }

    fn is_local_callable(&self, callee: &FunctionId) -> bool {
        self.builder
            .local_by_name
            .contains_key(callee.unqualified_name().as_deref().unwrap_or(""))
    }

    fn direct_call_supported(&self, callee: &FunctionId, args: &[HirExpr]) -> bool {
        !self.is_local_callable(callee)
            && self
                .builder
                .resolution
                .functions
                .get(callee)
                .is_some_and(|signature| {
                    !signature.is_async
                        && arguments_compatible(&self.builder.resolution, &signature.params, args)
                })
    }

    fn static_call_supported(&self, class: &TypeId, method: &str, args: &[HirExpr]) -> bool {
        static_signature(&self.builder.resolution, class, method).is_some_and(|signature| {
            signature.is_static
                && arguments_compatible(&self.builder.resolution, &signature.params, args)
        })
    }

    fn method_call_supported(&self, object: &HirExpr, method: &str, args: &[HirExpr]) -> bool {
        method_signature(&self.builder.resolution, &object.ty, method).is_some_and(|signature| {
            (!signature.is_static
                || matches!(&object.ty, Type::Named(name) | Type::Generic(name, _)
                    if self.builder.resolution.interfaces.contains_key(name)))
                && arguments_compatible(&self.builder.resolution, &signature.params, args)
        })
    }

    fn object_literal_supported(&self, class: &TypeId, fields: &[(String, HirExpr)]) -> bool {
        class_fields(&self.builder.resolution, class).is_some_and(|declared| {
            declared.len() == fields.len()
                && declared
                    .iter()
                    .all(|(name, _)| fields.iter().filter(|(field, _)| field == name).count() == 1)
        })
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
            self.pending
                .push(Work::Finish(expr, operands.len(), place, root));
            let reference = matches!(expr.kind, HirExprKind::ReferenceArg { .. });
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
        callee: FunctionId,
        index: usize,
    ) -> Option<()> {
        let HirExprKind::ReferenceArg { place } = &expr.kind else {
            unreachable!()
        };
        let captured = match &place.kind {
            HirExprKind::Var(name) => LirPlace::Local(*self.builder.local_by_name.get(name)?),
            HirExprKind::FieldAccess { object, field } => {
                let name = self.builder.synthetic_name("reference_object");
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
                }
            }
            HirExprKind::Index { .. } => {
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
                }
            }
            _ => return None,
        };
        let argument = LirOperand::Reference {
            place: captured,
            span: expr.span,
            display: reference_place_name(place),
        };
        self.builder.push_compute(
            LirRvalue::ReferenceDebug {
                argument: argument.clone(),
                callee,
                index,
            },
            Type::Void,
            expr.span,
            "reference_debug",
        );
        self.pending.push(Work::FinishReference(expr, argument));
        match &place.kind {
            HirExprKind::FieldAccess { object, .. } => {
                self.pending.push(Work::Eval(object, false, false))
            }
            HirExprKind::Index { array, index } => {
                self.pending.push(Work::Eval(index, false, false));
                self.pending.push(Work::Eval(array, false, false));
            }
            _ => {}
        }

        Some(())
    }

    fn finish_reference(&mut self, expr: &'e HirExpr, argument: LirOperand) -> Option<()> {
        let HirExprKind::ReferenceArg { place } = &expr.kind else {
            unreachable!()
        };
        let LirOperand::Reference {
            place: captured, ..
        } = &argument
        else {
            unreachable!()
        };
        match (&place.kind, captured) {
            (HirExprKind::Var(_), LirPlace::Local(_)) => {}
            (HirExprKind::FieldAccess { .. }, LirPlace::Field { object, .. }) => {
                let child = self.values.pop().expect("reference receiver");
                let value = self.builder.flat_operand(&child);
                self.builder.push(SourceInst::Compute {
                    local: *object,
                    value: LirRvalue::Use(value),
                    span: place.span,
                });
            }
            (HirExprKind::Index { .. }, LirPlace::ArrayElement { owner, index, .. }) => {
                let index_expr = self.values.pop().expect("reference index");
                let array_expr = self.values.pop().expect("reference array");
                let index_value = self.builder.flat_operand(&index_expr);
                let array_value = self.builder.flat_operand(&array_expr);
                self.builder.push(SourceInst::Compute {
                    local: *index,
                    value: LirRvalue::Use(index_value.clone()),
                    span: place.span,
                });
                self.builder.push(SourceInst::Compute {
                    local: *owner,
                    value: LirRvalue::CaptureArrayOwner {
                        array: array_value,
                        index: LirOperand::Local(*index),
                    },
                    span: place.span,
                });
            }
            _ => unreachable!(),
        }
        self.references.insert(std::ptr::from_ref(expr), argument);
        self.values.push(expr.clone());

        Some(())
    }

    fn direct_call(
        &mut self,
        expr: &'e HirExpr,
        callee: FunctionId,
        params: Vec<Type>,
        result: Type,
    ) -> Option<()> {
        let HirExprKind::Call { args, .. } = &expr.kind else {
            unreachable!()
        };
        let children = self.values.split_off(self.values.len() - args.len());
        let operands = args
            .iter()
            .zip(&children)
            .map(|(original, child)| {
                self.references
                    .remove(&std::ptr::from_ref(original))
                    .unwrap_or_else(|| self.builder.flat_operand(child))
            })
            .collect();
        let local = self.builder.push_compute(
            LirRvalue::DirectCall {
                callee,
                args: operands,
                params,
                result: result.clone(),
            },
            result,
            expr.span,
            "call_result",
        );
        self.values.push(self.builder.local_expr(local, expr.span));

        Some(())
    }

    fn static_call(&mut self, expr: &'e HirExpr) -> Option<()> {
        let HirExprKind::StaticCall {
            class,
            method,
            args,
        } = &expr.kind
        else {
            unreachable!()
        };
        let children = self.values.split_off(self.values.len() - args.len());
        let operands = args
            .iter()
            .zip(&children)
            .map(|(original, child)| {
                self.references
                    .remove(&std::ptr::from_ref(original))
                    .unwrap_or_else(|| self.builder.flat_operand(child))
            })
            .collect();
        let local = self.builder.push_compute(
            LirRvalue::StaticCall {
                class: *class,
                method: method.clone(),
                is_async: false,
                args: operands,
                arg_types: children.iter().map(|arg| arg.ty.clone()).collect(),
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
        expr: &'e HirExpr,
        callee: LirLocalId,
        params: Vec<Type>,
        result: Type,
    ) -> Option<()> {
        let HirExprKind::Call { callee: name, args } = &expr.kind else {
            unreachable!()
        };
        let children = self.values.split_off(self.values.len() - args.len());
        let args = children
            .iter()
            .map(|child| self.builder.flat_operand(child))
            .collect();
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
                name: *name,
                args,
                params,
                result,
            },
            span: expr.span,
        });
        self.values.push(self.builder.local_expr(local, expr.span));

        Some(())
    }

    fn prepare_method(&mut self, expr: &'e HirExpr) -> Option<()> {
        let HirExprKind::MethodCall {
            object,
            method,
            args,
            super_class,
        } = &expr.kind
        else {
            unreachable!()
        };
        let receiver = self.values.pop().expect("method receiver");
        let receiver = self.builder.flat_operand(&receiver);
        let name = self.builder.synthetic_name("method_receiver");
        let local = self.builder.declare_local(
            name,
            object.ty.clone(),
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
                receiver_ty: object.ty.clone(),
                method: method.clone(),
            },
            span: expr.span,
        });
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
        self.pending.push(Work::MethodCall(expr, local));
        let (Type::Named(owner) | Type::Generic(owner, _)) = &object.ty else {
            unreachable!()
        };
        self.pending.extend(
            arguments(
                FunctionId::method(super_class.unwrap_or(*owner), method),
                args,
                &method_signature(&self.builder.resolution, &object.ty, method)
                    .expect("method signature")
                    .params,
            )
            .into_iter()
            .rev(),
        );

        Some(())
    }

    fn method_call(&mut self, expr: &'e HirExpr, receiver: LirLocalId) -> Option<()> {
        let HirExprKind::MethodCall {
            object,
            method,
            args,
            super_class,
        } = &expr.kind
        else {
            unreachable!()
        };
        let children = self.values.split_off(self.values.len() - args.len());
        let args = args
            .iter()
            .zip(&children)
            .map(|(original, child)| {
                self.references
                    .remove(&std::ptr::from_ref(original))
                    .unwrap_or_else(|| self.builder.flat_operand(child))
            })
            .collect();
        let arg_types = children.iter().map(|child| child.ty.clone()).collect();
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
                receiver_ty: object.ty.clone(),
                method: method.clone(),
                // Filled once all method calls have been lowered.
                is_async: false,
                args,
                arg_types,
                result: expr.ty.clone(),
                super_class: *super_class,
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
        element: Type,
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
                element,
            },
            span,
        });

        Some(())
    }

    fn field_store(
        &mut self,
        object: LirLocalId,
        object_ty: Type,
        field: String,
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
                object_ty,
                field,
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
        variant: String,
        index: usize,
        enum_ty: Type,
        span: Span,
    ) -> Option<()> {
        let child = self.values.pop().expect("enum payload");
        let value = self.builder.flat_operand(&child);
        self.builder.push(SourceInst::Compute {
            local,
            value: LirRvalue::EnumPayloadStore {
                object: LirOperand::Local(local),
                class,
                variant,
                index,
                value,
                source: child.ty.clone(),
                enum_ty,
            },
            span,
        });

        Some(())
    }

    fn constructor(&mut self, object: LirLocalId, expr: &'e HirExpr) -> Option<()> {
        let HirExprKind::New { class, args } = &expr.kind else {
            unreachable!()
        };
        let children = self.values.split_off(self.values.len() - args.len());
        let operands = args
            .iter()
            .zip(&children)
            .map(|(original, child)| {
                self.references
                    .remove(&std::ptr::from_ref(original))
                    .unwrap_or_else(|| self.builder.flat_operand(child))
            })
            .collect();
        let arg_types = children.iter().map(|child| child.ty.clone()).collect();
        self.builder.push_compute(
            LirRvalue::ConstructorCall {
                object: LirOperand::Local(object),
                class: *class,
                args: operands,
                arg_types,
            },
            Type::Void,
            expr.span,
            "constructor_call",
        );

        Some(())
    }

    fn finish(&mut self, expr: &'e HirExpr, count: usize, place: bool, root: bool) -> Option<()> {
        let operands = self.values.split_off(self.values.len() - count);
        let lowered = expr.with_eager_operands(operands);
        if root || place || matches!(lowered.kind, HirExprKind::ReferenceArg { .. }) {
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
            Work::CoerceArgument(target) => self.coerce_argument(target),
            Work::CaptureReference(expr, callee, index) => {
                self.capture_reference(expr, callee, index)
            }
            Work::FinishReference(expr, argument) => self.finish_reference(expr, argument),
            Work::DirectCall(expr, callee, params, result) => {
                self.direct_call(expr, callee, params, result)
            }
            Work::StaticCall(expr) => self.static_call(expr),
            Work::FormatLiteral(local, text, first, span) => {
                self.format_literal(local, text, first, span)
            }
            Work::FormatValue(local, format, first, span) => {
                self.format_value(local, format, first, span)
            }
            Work::IndirectCall(expr, callee, params, result) => {
                self.indirect_call(expr, callee, params, result)
            }
            Work::PrepareMethod(expr) => self.prepare_method(expr),
            Work::MethodCall(expr, receiver) => self.method_call(expr, receiver),
            Work::ArrayStore(array, index, element, span) => {
                self.array_store(array, index, element, span)
            }
            Work::FieldStore(object, object_ty, field, span) => {
                self.field_store(object, object_ty, field, span)
            }
            Work::EnumStore(local, class, variant, index, enum_ty, span) => {
                self.enum_store(local, class, variant, index, enum_ty, span)
            }
            Work::Constructor(object, expr) => self.constructor(object, expr),
            Work::Finish(expr, count, place, root) => self.finish(expr, count, place, root),
        }
    }
}
