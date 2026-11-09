use super::*;

pub(super) fn lower(
    expr: &mut HirExpr,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &mut HashMap<String, LirLocalId>,
    callables: &Callables,
    is_async: bool,
) {
    if !matches!(
        expr.kind,
        HirExprKind::Unary { .. }
            | HirExprKind::Binary { .. }
            | HirExprKind::Invoke { .. }
            | HirExprKind::Call { .. }
            | HirExprKind::Await { .. }
            | HirExprKind::New { .. }
            | HirExprKind::ObjectLiteral { .. }
            | HirExprKind::StaticField { .. }
            | HirExprKind::StaticCall { .. }
            | HirExprKind::Array { .. }
            | HirExprKind::Index { .. }
            | HirExprKind::FieldAccess { .. }
            | HirExprKind::Range { .. }
            | HirExprKind::Str(_)
            | HirExprKind::Print { .. }
            | HirExprKind::FnRef(_)
            | HirExprKind::Lambda { .. }
            | HirExprKind::MethodCall { .. }
    ) || !scalar(expr, names, locals, callables, is_async)
    {
        return;
    }
    let snapshots = expr.walk_postorder(false).any(|node| {
        matches!(
            node.kind,
            HirExprKind::Call { .. }
                | HirExprKind::Invoke { .. }
                | HirExprKind::MethodCall { .. }
                | HirExprKind::New { .. }
                | HirExprKind::StaticCall { .. }
        )
    });

    let mut context = ScalarLowering {
        emitted,
        locals,
        names,
        callables,
        is_async,
        snapshots,
        values: HashMap::new(),
        reference_sites: HashMap::new(),
        work: vec![Work::Enter(&*expr)],
    };
    while let Some(action) = context.work.pop() {
        if let Some(kind) = context.step(action) {
            expr.kind = kind;
            expr.ty = Type::Never;
            return;
        }
    }
    let LirOperand::Local(result) = context.values[&std::ptr::from_ref(&*expr)] else {
        unreachable!()
    };
    expr.kind = HirExprKind::Var(context.locals[result.0 as usize].name.clone());
}

enum Work<'a> {
    Enter(&'a HirExpr),
    Coerce {
        child: &'a HirExpr,
        target: Type,
    },
    Finish(&'a HirExpr),
    AwaitBuiltin(&'a HirExpr),
    Reference(&'a HirExpr),
    PrepareMethod(&'a HirExpr),
    FieldStore {
        object: LirOperand,
        object_ty: Type,
        field: String,
        child: &'a HirExpr,
    },
    EnumStore {
        node: &'a HirExpr,
        class: TypeId,
        variant: String,
        index: usize,
        child: &'a HirExpr,
    },
    Raise {
        node: &'a HirExpr,
        message: Option<&'a HirExpr>,
    },
    FormatLiteral {
        node: &'a HirExpr,
        text: String,
    },
    FormatValue {
        node: &'a HirExpr,
        child: &'a HirExpr,
        format: Option<crate::interpolate::F64Format>,
    },
    ArrayStore {
        array: LirOperand,
        index: usize,
        element: Type,
        child: &'a HirExpr,
    },
}

fn arguments<'a>(args: &'a [HirExpr], targets: Option<&[Type]>) -> Vec<Work<'a>> {
    let mut work = Vec::new();
    for (index, child) in args.iter().enumerate().rev() {
        if let Some(target) = targets.and_then(|types| types.get(index))
            && *target != child.ty
            && !matches!(child.kind, HirExprKind::ReferenceArg { .. })
        {
            work.push(Work::Coerce {
                child,
                target: target.clone(),
            });
        }
        work.push(Work::Enter(child));
    }
    work
}

/// Per-expression state shared by the iterative scalar lowering actions.
struct ScalarLowering<'a, 'e> {
    emitted: &'a mut Vec<SourceInst>,
    locals: &'a mut Vec<LirLocal>,
    names: &'a mut HashMap<String, LirLocalId>,
    callables: &'a Callables,
    is_async: bool,
    snapshots: bool,
    values: HashMap<*const HirExpr, LirOperand>,
    reference_sites: HashMap<*const HirExpr, (FunctionId, usize)>,
    work: Vec<Work<'e>>,
}

impl<'e> ScalarLowering<'_, 'e> {
    fn emit(&mut self, value: LirRvalue, ty: Type, span: Span) -> LirLocalId {
        emit(value, ty, span, self.emitted, self.locals, self.names)
    }

    fn finish_await(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Await { inner } = &node.kind else {
            unreachable!()
        };
        LirOperand::Local(self.emit(
            LirRvalue::AwaitFuture {
                future: self.values[&std::ptr::from_ref(&**inner)].clone(),
                result: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_static_call(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::StaticCall {
            class,
            method,
            args,
        } = &node.kind
        else {
            unreachable!()
        };
        LirOperand::Local(
            self.emit(
                LirRvalue::StaticCall {
                    class: *class,
                    method: method.clone(),
                    is_async: false,
                    args: args
                        .iter()
                        .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
                        .collect(),
                    arg_types: args
                        .iter()
                        .map(|arg| {
                            self.values[&std::ptr::from_ref(arg)]
                                .ty(self.locals)
                                .expect("typed argument")
                        })
                        .collect(),
                    result: node.ty.clone(),
                },
                node.ty.clone(),
                node.span,
            ),
        )
    }

    fn finish_new(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::New { class, args } = &node.kind else {
            unreachable!()
        };

        let object = self.values[&std::ptr::from_ref(node)].clone();
        self.emit(
            LirRvalue::ConstructorCall {
                object: object.clone(),
                class: *class,
                args: args
                    .iter()
                    .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
                    .collect(),
                arg_types: args
                    .iter()
                    .map(|arg| {
                        self.values[&std::ptr::from_ref(arg)]
                            .ty(self.locals)
                            .expect("typed argument")
                    })
                    .collect(),
            },
            Type::Void,
            node.span,
        );
        object
    }

    fn finish_index(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Index { array, index } = &node.kind else {
            unreachable!()
        };
        LirOperand::Local(self.emit(
            LirRvalue::Index {
                array: self.values[&std::ptr::from_ref(&**array)].clone(),
                index: self.values[&std::ptr::from_ref(&**index)].clone(),
                element: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_field_access(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::FieldAccess { object, field } = &node.kind else {
            unreachable!()
        };
        LirOperand::Local(self.emit(
            LirRvalue::FieldLoad {
                object: self.values[&std::ptr::from_ref(&**object)].clone(),
                object_ty: object.ty.clone(),
                field: field.clone(),
                result: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_static_field(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::StaticField { class, field } = &node.kind else {
            unreachable!()
        };

        let value = if self.callables.resolution.enums.contains_key(class) {
            LirRvalue::EnumAlloc {
                class: *class,
                variant: field.clone(),
                enum_ty: node.ty.clone(),
            }
        } else {
            LirRvalue::StaticField {
                class: *class,
                field: field.clone(),
                result: node.ty.clone(),
            }
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_range(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Range { start, end } = &node.kind else {
            unreachable!()
        };
        LirOperand::Local(self.emit(
            LirRvalue::Range {
                start: self.values[&std::ptr::from_ref(&**start)].clone(),
                end: self.values[&std::ptr::from_ref(&**end)].clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_int(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Int(value) = &node.kind else {
            unreachable!()
        };
        LirOperand::Int(*value)
    }

    fn finish_float(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Float(value) = &node.kind else {
            unreachable!()
        };
        LirOperand::Float(*value)
    }

    fn finish_bool(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Bool(value) = &node.kind else {
            unreachable!()
        };
        LirOperand::Bool(*value)
    }

    fn finish_str(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Str(value) = &node.kind else {
            unreachable!()
        };
        LirOperand::Local(self.emit(
            LirRvalue::StringLiteral(value.clone()),
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_fn_ref(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::FnRef(function) = &node.kind else {
            unreachable!()
        };
        LirOperand::Local(self.emit(
            LirRvalue::FunctionRef {
                function: *function,
                ty: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_lambda(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Lambda { id, captures, .. } = &node.kind else {
            unreachable!()
        };

        let value = LirRvalue::Closure {
            id: *id,
            captures: captures
                .iter()
                .map(|capture| LirOperand::Local(self.names[&capture.source]))
                .collect(),
            ty: node.ty.clone(),
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_print(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Print { value, newline } = &node.kind else {
            unreachable!()
        };

        let value = LirRvalue::Print {
            value: self.values[&std::ptr::from_ref(&**value)].clone(),
            ty: value.ty.clone(),
            newline: *newline,
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_var(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Var(name) = &node.kind else {
            unreachable!()
        };

        let source = LirOperand::Local(self.names[name]);
        if self.snapshots {
            LirOperand::Local(self.emit(LirRvalue::Use(source), node.ty.clone(), node.span))
        } else {
            source
        }
    }

    fn finish_method_call(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::MethodCall {
            object,
            method,
            args,
            super_class,
        } = &node.kind
        else {
            unreachable!()
        };

        let receiver = self.values[&std::ptr::from_ref(&**object)].clone();
        let operands = args
            .iter()
            .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
            .collect();
        let arg_types = args
            .iter()
            .map(|arg| {
                self.values[&std::ptr::from_ref(arg)]
                    .ty(self.locals)
                    .expect("typed argument")
            })
            .collect();
        let value = if let Some(resolved) =
            crate::semantic::intrinsics::resolve(&object.ty, method, args.len())
        {
            LirRvalue::IntrinsicCall {
                intrinsic: resolved.intrinsic,
                method: method.clone(),
                receiver,
                receiver_ty: object.ty.clone(),
                args: operands,
                arg_types,
                result: node.ty.clone(),
            }
        } else if enum_method(&object.ty, method) {
            LirRvalue::EnumMethod {
                receiver,
                receiver_ty: object.ty.clone(),
                method: method.clone(),
                args: operands,
                arg_types,
                result: node.ty.clone(),
            }
        } else {
            LirRvalue::MethodCall {
                receiver,
                receiver_ty: object.ty.clone(),
                method: method.clone(),
                // Filled in the common final call walk below.
                is_async: false,
                args: operands,
                arg_types,
                result: node.ty.clone(),
                super_class: *super_class,
            }
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_invoke(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Invoke { name, callee, args } = &node.kind else {
            unreachable!()
        };

        let (Type::Fn(params, result) | Type::Closure(params, result)) = &callee.ty else {
            unreachable!()
        };
        let value = LirRvalue::IndirectCall {
            callee: self.values[&std::ptr::from_ref(&**callee)].clone(),
            name: *name,
            args: args
                .iter()
                .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
                .collect(),
            params: params.clone(),
            result: (**result).clone(),
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_call(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Call { callee, args } = &node.kind else {
            unreachable!()
        };

        let operands = args
            .iter()
            .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
            .collect();
        let value = if let Some(callable) = self.values.get(&std::ptr::from_ref(node)) {
            let ty = callable.ty(self.locals).expect("callable local");
            let (Type::Fn(params, result) | Type::Closure(params, result)) = &ty else {
                unreachable!()
            };
            LirRvalue::IndirectCall {
                callee: callable.clone(),
                name: *callee,
                args: operands,
                params: params.clone(),
                result: (**result).clone(),
            }
        } else if let Some((params, result)) = self.callables.get(callee) {
            if let Some(output) = self.callables.async_outputs.get(callee) {
                LirRvalue::StartTask {
                    callee: *callee,
                    args: operands,
                    params: params.clone(),
                    output: output.clone(),
                }
            } else {
                LirRvalue::DirectCall {
                    callee: *callee,
                    args: operands,
                    params: params.clone(),
                    result: result.clone(),
                }
            }
        } else if callee.is_free_named("recover") {
            LirRvalue::Recover
        } else {
            LirRvalue::BuiltinCall {
                callee: *callee,
                args: operands,
                params: args.iter().map(|arg| arg.ty.clone()).collect(),
                result: node.ty.clone(),
            }
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_unary(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Unary { op, operand } = &node.kind else {
            unreachable!()
        };

        let value = LirRvalue::Unary {
            op: op.clone(),
            operand: self.values[&std::ptr::from_ref(&**operand)].clone(),
            ty: node.ty.clone(),
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_binary(&mut self, node: &HirExpr) -> LirOperand {
        let HirExprKind::Binary { op, lhs, rhs } = &node.kind else {
            unreachable!()
        };

        let lhs_value = self.values[&std::ptr::from_ref(&**lhs)].clone();
        let value = LirRvalue::Binary {
            op: op.clone(),
            lhs: lhs_value.clone(),
            rhs: self.values[&std::ptr::from_ref(&**rhs)].clone(),
            // A mixed class/interface identity comparison coerced
            // its subtype side on entry, so the left operand already
            // carries the shared type.
            operand_ty: if lhs.ty == rhs.ty {
                lhs.ty.clone()
            } else {
                lhs_value.ty(self.locals).unwrap_or_else(|| lhs.ty.clone())
            },
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish(&mut self, node: &HirExpr) {
        let operand = match &node.kind {
            HirExprKind::Await { .. } => self.finish_await(node),
            HirExprKind::StaticCall { .. } => self.finish_static_call(node),
            HirExprKind::New { .. } => self.finish_new(node),
            HirExprKind::Index { .. } => self.finish_index(node),
            HirExprKind::FieldAccess { .. } => self.finish_field_access(node),
            HirExprKind::StaticField { .. } => self.finish_static_field(node),
            HirExprKind::Range { .. } => self.finish_range(node),
            HirExprKind::Int(_) => self.finish_int(node),
            HirExprKind::Float(_) => self.finish_float(node),
            HirExprKind::Bool(_) => self.finish_bool(node),
            HirExprKind::Str(_) => self.finish_str(node),
            HirExprKind::FnRef(_) => self.finish_fn_ref(node),
            HirExprKind::Lambda { .. } => self.finish_lambda(node),
            HirExprKind::Print { .. } => self.finish_print(node),
            HirExprKind::Var(_) => self.finish_var(node),
            HirExprKind::MethodCall { .. } => self.finish_method_call(node),
            HirExprKind::Invoke { .. } => self.finish_invoke(node),
            HirExprKind::Call { .. } => self.finish_call(node),
            HirExprKind::Unary { .. } => self.finish_unary(node),
            HirExprKind::Binary { .. } => self.finish_binary(node),
            _ => unreachable!("scalar tree was validated"),
        };
        self.values.insert(std::ptr::from_ref(node), operand);
    }

    fn enter_await(&mut self, node: &'e HirExpr, targets: Option<&[Type]>) -> bool {
        let HirExprKind::Await { inner } = &node.kind else {
            unreachable!()
        };

        let HirExprKind::Call { args, .. } = &inner.kind else {
            unreachable!()
        };
        self.work.push(Work::AwaitBuiltin(node));
        self.work.extend(arguments(args, targets));
        true
    }

    fn enter_reference_arg(&mut self, node: &'e HirExpr) -> bool {
        let HirExprKind::ReferenceArg { place } = &node.kind else {
            unreachable!()
        };

        let captured = match &place.kind {
            HirExprKind::Var(name) => LirPlace::Local(self.names[name]),
            HirExprKind::FieldAccess { object, field } => LirPlace::Field {
                object: reserve(object.ty.clone(), self.locals, self.names),
                object_ty: object.ty.clone(),
                field: field.clone(),
                ty: place.ty.clone(),
            },
            HirExprKind::Index { .. } => {
                let owner = reserve(Type::Void, self.locals, self.names);
                self.locals[owner.0 as usize].storage_kind =
                    super::super::super::LirStorageKind::GcOwner;
                LirPlace::ArrayElement {
                    owner,
                    index: reserve(Type::I64, self.locals, self.names),
                    element: place.ty.clone(),
                }
            }
            _ => unreachable!("validated reference place"),
        };
        let argument = LirOperand::Reference {
            place: captured,
            span: node.span,
            display: super::super::super::reference_place_name(place),
        };
        if let Some((callee, index)) = self.reference_sites.get(&std::ptr::from_ref(node)) {
            self.emit(
                LirRvalue::ReferenceDebug {
                    argument: argument.clone(),
                    callee: *callee,
                    index: *index,
                },
                Type::Void,
                node.span,
            );
        }
        self.values.insert(std::ptr::from_ref(node), argument);
        self.work.push(Work::Reference(node));
        match &place.kind {
            HirExprKind::Var(_) => {}
            HirExprKind::FieldAccess { object, .. } => self.work.push(Work::Enter(object)),
            HirExprKind::Index { array, index } => {
                self.work.push(Work::Enter(index));
                self.work.push(Work::Enter(array));
            }
            _ => unreachable!("validated reference place"),
        }
        true
    }

    fn enter_method_call(&mut self, node: &'e HirExpr, targets: Option<&[Type]>) -> bool {
        let HirExprKind::MethodCall { object, args, .. } = &node.kind else {
            unreachable!()
        };

        self.work.push(Work::Finish(node));
        self.work.extend(arguments(args, targets));
        self.work.push(Work::PrepareMethod(node));
        self.work.push(Work::Enter(object));
        true
    }

    fn enter_enum(&mut self, node: &'e HirExpr) -> bool {
        let HirExprKind::StaticCall {
            class,
            method,
            args,
        } = &node.kind
        else {
            unreachable!()
        };

        let object = LirOperand::Local(self.emit(
            LirRvalue::EnumAlloc {
                class: *class,
                variant: method.clone(),
                enum_ty: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ));
        self.values.insert(std::ptr::from_ref(node), object);
        for (index, child) in args.iter().enumerate().rev() {
            self.work.push(Work::EnumStore {
                node,
                class: *class,
                variant: method.clone(),
                index,
                child,
            });
            self.work.push(Work::Enter(child));
        }
        true
    }

    fn enter_new(&mut self, node: &'e HirExpr, targets: Option<&[Type]>) -> bool {
        let HirExprKind::New { class, args } = &node.kind else {
            unreachable!()
        };

        let object = LirOperand::Local(self.emit(
            LirRvalue::ObjectAlloc { class: *class },
            node.ty.clone(),
            node.span,
        ));
        self.values.insert(std::ptr::from_ref(node), object.clone());
        if self.callables.resolution.classes[class]
            .constructor
            .is_some()
        {
            if args
                .iter()
                .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
            {
                self.emit(LirRvalue::BeginReferenceCall, Type::Void, node.span);
            }
            self.work.push(Work::Finish(node));
            self.work.extend(arguments(args, targets));
        } else {
            let fields = class_fields(class, self.callables).expect("validated class");
            for ((field, _), child) in fields.into_iter().zip(args).rev() {
                self.work.push(Work::FieldStore {
                    object: object.clone(),
                    object_ty: node.ty.clone(),
                    field,
                    child,
                });
                self.work.push(Work::Enter(child));
            }
        }
        true
    }

    fn enter_object_literal(&mut self, node: &'e HirExpr) -> bool {
        let HirExprKind::ObjectLiteral { class, fields } = &node.kind else {
            unreachable!()
        };

        let object = LirOperand::Local(self.emit(
            LirRvalue::ObjectAlloc { class: *class },
            node.ty.clone(),
            node.span,
        ));
        self.values.insert(std::ptr::from_ref(node), object.clone());
        for (field, child) in fields.iter().rev() {
            self.work.push(Work::FieldStore {
                object: object.clone(),
                object_ty: node.ty.clone(),
                field: field.clone(),
                child,
            });
            self.work.push(Work::Enter(child));
        }
        true
    }

    fn enter_special(&mut self, node: &'e HirExpr, targets: Option<&[Type]>) -> bool {
        match &node.kind {
            HirExprKind::Await { inner }
                if !self.is_async
                    && matches!(&inner.kind, HirExprKind::Call { callee, .. } if (callee.is_free_named("sleep") || callee.is_free_named("yield")) && !self.names.contains_key(callee.unqualified_name().as_deref().unwrap_or("")) && !self.callables.contains_key(callee)) =>
            {
                self.enter_await(node, targets)
            }
            HirExprKind::ReferenceArg { .. } => self.enter_reference_arg(node),
            HirExprKind::MethodCall {
                object,
                method,
                args,
                super_class,
            } if instance_method(
                super_class.map(Type::Named).as_ref().unwrap_or(&object.ty),
                method,
                args,
                self.callables,
            ) =>
            {
                self.enter_method_call(node, targets)
            }
            HirExprKind::StaticCall { class, .. }
                if self.callables.resolution.enums.contains_key(class) =>
            {
                self.enter_enum(node)
            }
            HirExprKind::New { .. } => self.enter_new(node, targets),
            HirExprKind::ObjectLiteral { .. } => self.enter_object_literal(node),
            _ => false,
        }
    }

    fn prepare_references(&mut self, node: &HirExpr) {
        let reference_call = match &node.kind {
            HirExprKind::Call { callee, args } => Some((*callee, args)),
            HirExprKind::MethodCall {
                object,
                method,
                args,
                super_class,
            } => match &object.ty {
                Type::Named(owner) | Type::Generic(owner, _) => Some((
                    FunctionId::method(super_class.unwrap_or(*owner), method),
                    args,
                )),
                _ => None,
            },
            HirExprKind::New { class, args } => Some((FunctionId::method(*class, "init"), args)),
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } => Some((FunctionId::method(*class, method), args)),
            _ => None,
        };
        if let Some((callee, args)) = reference_call {
            let has_refs = args
                .iter()
                .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }));
            for (index, arg) in args.iter().enumerate() {
                if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                    self.reference_sites
                        .insert(std::ptr::from_ref(arg), (callee, index));
                }
            }
            if has_refs
                && !matches!(
                    node.kind,
                    HirExprKind::MethodCall { .. } | HirExprKind::New { .. }
                )
            {
                self.emit(LirRvalue::BeginReferenceCall, Type::Void, node.span);
            }
        }
    }

    fn enter_call(&mut self, node: &'e HirExpr, targets: Option<&[Type]>) -> bool {
        let HirExprKind::Call { callee, args } = &node.kind else {
            return false;
        };

        if let Some(source) = self
            .names
            .get(callee.unqualified_name().as_deref().unwrap_or(""))
            .copied()
        {
            let ty = self.locals[source.0 as usize].ty.clone();
            let snapshot = self.emit(LirRvalue::Use(LirOperand::Local(source)), ty, node.span);
            self.values
                .insert(std::ptr::from_ref(node), LirOperand::Local(snapshot));
            self.work.push(Work::Finish(node));
            self.work.extend(arguments(args, targets));
            return true;
        }
        let panic = callee.is_free_named("panic") && !self.callables.contains_key(callee);
        if panic {
            let message = if args.len() == 1 { args.first() } else { None };
            self.work.push(Work::Raise { node, message });
            if args.is_empty() {
                self.work.push(Work::FormatLiteral {
                    node,
                    text: "explicit panic".into(),
                });
                return true;
            }
            if let Some(message) = message {
                self.work.push(Work::Enter(message));
                return true;
            }
        }
        if (callee.is_free_named("format") || panic) && !self.callables.contains_key(callee) {
            let segments = format_segments(args).expect("validated format");
            let mut actions = Vec::new();
            let mut operands = args[1..].iter();
            if segments.is_empty() {
                actions.push(Work::FormatLiteral {
                    node,
                    text: String::new(),
                });
            }
            for segment in segments {
                match segment {
                    crate::interpolate::Segment::Literal(text) => {
                        actions.push(Work::FormatLiteral { node, text })
                    }
                    segment => {
                        let child = operands.next().expect("validated placeholder");
                        let format = match segment {
                            crate::interpolate::Segment::F64(format) => Some(format),
                            _ => None,
                        };
                        actions.push(Work::Enter(child));
                        actions.push(Work::FormatValue {
                            node,
                            child,
                            format,
                        });
                    }
                }
            }
            self.work.extend(actions.into_iter().rev());
            return true;
        }

        false
    }

    fn enter(&mut self, node: &'e HirExpr) -> Option<HirExprKind> {
        // A CFG expression with only divergent branches leaves a
        // Never local in its unreachable merge block, not a value.
        if node.ty == Type::Never && matches!(node.kind, HirExprKind::Var(_)) {
            return Some(node.kind.clone());
        }
        let targets = argument_types(node, self.names, self.locals, self.callables);
        self.prepare_references(node);
        if self.enter_special(node, targets.as_deref()) {
            return None;
        }
        if self.enter_call(node, targets.as_deref()) {
            return None;
        }
        if let HirExprKind::Array { elements } = &node.kind {
            let Type::Array(element) = &node.ty else {
                unreachable!()
            };
            let array = LirOperand::Local(self.emit(
                LirRvalue::ArrayAlloc {
                    length: elements.len(),
                    element: (**element).clone(),
                },
                node.ty.clone(),
                node.span,
            ));
            self.values.insert(std::ptr::from_ref(node), array.clone());
            for (index, child) in elements.iter().enumerate().rev() {
                self.work.push(Work::ArrayStore {
                    array: array.clone(),
                    index,
                    element: (**element).clone(),
                    child,
                });
                self.work.push(Work::Enter(child));
            }
            return None;
        }
        self.work.push(Work::Finish(node));
        if let HirExprKind::Invoke { callee, args, .. } = &node.kind {
            self.work.extend(arguments(args, targets.as_deref()));
            self.work.push(Work::Enter(callee));
        } else if let HirExprKind::Call { args, .. } | HirExprKind::StaticCall { args, .. } =
            &node.kind
        {
            self.work.extend(arguments(args, targets.as_deref()));
        } else if let HirExprKind::Binary { lhs, rhs, .. } = &node.kind
            && lhs.ty != rhs.ty
            && let Some(target) = identity_operand_type(lhs, rhs, &self.callables.resolution)
        {
            for child in [&**rhs, &**lhs] {
                if child.ty != target {
                    self.work.push(Work::Coerce {
                        child,
                        target: target.clone(),
                    });
                }
                self.work.push(Work::Enter(child));
            }
        } else if !matches!(node.kind, HirExprKind::Lambda { .. }) {
            self.work
                .extend(node.children().into_iter().rev().map(Work::Enter));
        }
        None
    }

    fn coerce(&mut self, child: &'e HirExpr, target: Type) {
        let operand = self.values[&std::ptr::from_ref(child)].clone();
        let source = operand.ty(self.locals).expect("typed argument");
        let local = self.emit(
            LirRvalue::Coerce {
                value: operand,
                source,
                target: target.clone(),
            },
            target,
            child.span,
        );
        self.values
            .insert(std::ptr::from_ref(child), LirOperand::Local(local));
    }

    fn await_builtin(&mut self, node: &'e HirExpr) {
        let HirExprKind::Await { inner } = &node.kind else {
            unreachable!()
        };
        let HirExprKind::Call { callee, args } = &inner.kind else {
            unreachable!()
        };
        let future_type = Type::Generic(TypeId::local("Future"), vec![Type::Void]);
        let future = self.emit(
            LirRvalue::BuiltinCall {
                callee: *callee,
                args: args
                    .iter()
                    .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
                    .collect(),
                params: args.iter().map(|arg| arg.ty.clone()).collect(),
                result: future_type.clone(),
            },
            future_type,
            inner.span,
        );
        let result = self.emit(
            LirRvalue::AwaitFuture {
                future: LirOperand::Local(future),
                result: Type::Void,
            },
            Type::Void,
            node.span,
        );
        self.values
            .insert(std::ptr::from_ref(node), LirOperand::Local(result));
    }

    fn reference(&mut self, node: &'e HirExpr) {
        let HirExprKind::ReferenceArg { place } = &node.kind else {
            unreachable!()
        };
        let LirOperand::Reference {
            place: captured, ..
        } = self.values[&std::ptr::from_ref(node)].clone()
        else {
            unreachable!()
        };
        match (&place.kind, captured) {
            (HirExprKind::Var(_), LirPlace::Local(_)) => {}
            (HirExprKind::FieldAccess { object, .. }, LirPlace::Field { object: local, .. }) => {
                self.emitted.push(SourceInst::Compute {
                    local,
                    value: LirRvalue::Use(self.values[&std::ptr::from_ref(&**object)].clone()),
                    span: object.span,
                });
            }
            (
                HirExprKind::Index { array, index },
                LirPlace::ArrayElement {
                    owner,
                    index: local,
                    ..
                },
            ) => {
                let index_value = self.values[&std::ptr::from_ref(&**index)].clone();
                self.emitted.push(SourceInst::Compute {
                    local,
                    value: LirRvalue::Use(index_value.clone()),
                    span: index.span,
                });
                self.emitted.push(SourceInst::Compute {
                    local: owner,
                    value: LirRvalue::CaptureArrayOwner {
                        array: self.values[&std::ptr::from_ref(&**array)].clone(),
                        index: LirOperand::Local(local),
                    },
                    span: place.span,
                });
            }
            _ => unreachable!("reserved reference place"),
        }
    }

    fn prepare_method(&mut self, node: &'e HirExpr) {
        let HirExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &node.kind
        else {
            unreachable!()
        };
        let receiver = self.values[&std::ptr::from_ref(&**object)].clone();
        let local = self.emit(
            LirRvalue::PrepareMethod {
                receiver,
                receiver_ty: object.ty.clone(),
                method: method.clone(),
            },
            object.ty.clone(),
            node.span,
        );
        self.values
            .insert(std::ptr::from_ref(&**object), LirOperand::Local(local));
        if args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.emit(LirRvalue::BeginReferenceCall, Type::Void, node.span);
        }
    }

    fn field_store(
        &mut self,
        object: LirOperand,
        object_ty: Type,
        field: String,
        child: &'e HirExpr,
    ) {
        self.emit(
            LirRvalue::FieldStore {
                object,
                object_ty,
                field,
                value: self.values[&std::ptr::from_ref(child)].clone(),
            },
            Type::Void,
            child.span,
        );
    }

    fn enum_store(
        &mut self,
        node: &'e HirExpr,
        class: TypeId,
        variant: String,
        index: usize,
        child: &'e HirExpr,
    ) {
        let object = self.values[&std::ptr::from_ref(node)].clone();
        let LirOperand::Local(local) = object else {
            unreachable!()
        };
        self.emitted.push(SourceInst::Compute {
            local,
            value: LirRvalue::EnumPayloadStore {
                object,
                class,
                variant,
                index,
                value: self.values[&std::ptr::from_ref(child)].clone(),
                source: child.ty.clone(),
                enum_ty: node.ty.clone(),
            },
            span: child.span,
        });
    }

    fn raise(&mut self, node: &'e HirExpr, message: Option<&'e HirExpr>) -> Option<HirExprKind> {
        let message = message
            .map(|child| self.values[&std::ptr::from_ref(child)].clone())
            .unwrap_or_else(|| self.values[&std::ptr::from_ref(node)].clone());
        let result = self.emit(LirRvalue::Panic { message }, Type::Never, node.span);
        // No enclosing operand, coercion, store, or later argument can
        // execute after this raise. Preserve only the evaluated prefix.
        Some(HirExprKind::Var(
            self.locals[result.0 as usize].name.clone(),
        ))
    }

    fn format_literal(&mut self, node: &'e HirExpr, text: String) {
        let piece =
            LirOperand::Local(self.emit(LirRvalue::StringLiteral(text), Type::String, node.span));
        append_format(
            node,
            piece,
            &mut self.values,
            self.emitted,
            self.locals,
            self.names,
        );
    }

    fn format_value(
        &mut self,
        node: &'e HirExpr,
        child: &'e HirExpr,
        format: Option<crate::interpolate::F64Format>,
    ) {
        let piece = LirOperand::Local(self.emit(
            LirRvalue::FormatScalar {
                value: self.values[&std::ptr::from_ref(child)].clone(),
                ty: child.ty.clone(),
                format,
            },
            Type::String,
            child.span,
        ));
        append_format(
            node,
            piece,
            &mut self.values,
            self.emitted,
            self.locals,
            self.names,
        );
    }

    fn array_store(&mut self, array: LirOperand, index: usize, element: Type, child: &'e HirExpr) {
        self.emit(
            LirRvalue::ArrayStore {
                array,
                index: LirOperand::Int(index as i64),
                value: self.values[&std::ptr::from_ref(child)].clone(),
                element,
            },
            Type::Void,
            child.span,
        );
    }

    // A divergent action returns the replacement root and stops the worklist.
    // Ordinary actions leave the root untouched until all operands are ready.
    fn step(&mut self, action: Work<'e>) -> Option<HirExprKind> {
        match action {
            Work::Coerce { child, target } => {
                self.coerce(child, target);
                None
            }
            Work::AwaitBuiltin(node) => {
                self.await_builtin(node);
                None
            }
            Work::Reference(node) => {
                self.reference(node);
                None
            }
            Work::PrepareMethod(node) => {
                self.prepare_method(node);
                None
            }
            Work::FieldStore {
                object,
                object_ty,
                field,
                child,
            } => {
                self.field_store(object, object_ty, field, child);
                None
            }
            Work::EnumStore {
                node,
                class,
                variant,
                index,
                child,
            } => {
                self.enum_store(node, class, variant, index, child);
                None
            }
            Work::Raise { node, message } => self.raise(node, message),
            Work::FormatLiteral { node, text } => {
                self.format_literal(node, text);
                None
            }
            Work::FormatValue {
                node,
                child,
                format,
            } => {
                self.format_value(node, child, format);
                None
            }
            Work::ArrayStore {
                array,
                index,
                element,
                child,
            } => {
                self.array_store(array, index, element, child);
                None
            }
            Work::Finish(node) => {
                self.finish(node);
                None
            }
            Work::Enter(node) => self.enter(node),
        }
    }
}
