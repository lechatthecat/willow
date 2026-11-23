use super::*;
use crate::ir::lowered::MethodOperands;
use crate::ir::typed_ast::HirCapture;
use crate::semantic::intrinsics::BuiltinMethod;
use std::borrow::Cow;

#[cfg(test)]
std::thread_local! {
    static PEAK_MATERIALIZED_TARGET_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn lower(
    expr: &mut HirExpr,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &HashMap<String, LirLocalId>,
    callables: &Callables,
    is_async: bool,
) {
    if !needs_lowering(expr) {
        return;
    }
    let mut plan = ScalarPlan::default();
    let valid = scalar(expr, names, locals, callables, is_async, &mut plan);
    if !valid {
        return;
    }
    let result = lower_validated(expr, emitted, locals, names, callables, is_async, &mut plan);
    drop(plan);
    let local = match result {
        Ok(local) => local,
        Err(local) => {
            expr.ty = Type::Never;
            local
        }
    };
    expr.kind = HirExprKind::Var(locals[local.0 as usize].name.clone());
}

pub(super) fn needs_lowering(expr: &HirExpr) -> bool {
    matches!(
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
    )
}

pub(super) fn lower_validated<'d>(
    expr: &'d HirExpr,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &HashMap<String, LirLocalId>,
    callables: &'d Callables,
    is_async: bool,
    plan: &mut ScalarPlan<'d>,
) -> Result<LirLocalId, LirLocalId> {
    let mut context = ScalarLowering {
        emitted,
        locals,
        names,
        callables,
        is_async,
        snapshots: plan.snapshots,
        plan,
        values: HashMap::new(),
        reference_sites: HashMap::new(),
        work: vec![Work::Enter(expr)],
    };
    while let Some(action) = context.work.pop() {
        if let Some(stop) = context.step(action) {
            return Err(stop);
        }
    }
    let LirOperand::Local(result) = context.values[&std::ptr::from_ref(expr)] else {
        unreachable!("a completed scalar root is stored in a result local")
    };
    Ok(result)
}

enum Work<'a> {
    Enter(&'a HirExpr),
    Coerce {
        child: &'a HirExpr,
        target: Cow<'a, Type>,
    },
    CoerceProjected {
        child: &'a HirExpr,
        target: ProjectedParameter<'a>,
    },
    CoerceSnapshot {
        child: &'a HirExpr,
        snapshot: LirLocalId,
        index: usize,
    },
    Finish(&'a HirExpr, Finish<'a>),
    AwaitBuiltin {
        node: &'a HirExpr,
        call_span: &'a Span,
        callee: FunctionId,
        args: &'a [HirExpr],
    },
    CaptureField {
        object: &'a HirExpr,
        local: LirLocalId,
    },
    CaptureElement {
        array: &'a HirExpr,
        index: &'a HirExpr,
        owner: LirLocalId,
        slot: LirLocalId,
        span: &'a Span,
    },
    PrepareMethod(&'a HirExpr, MethodOperands<'a>),
    FieldStore {
        object: LirLocalId,
        object_ty: &'a Type,
        field: &'a str,
        child: &'a HirExpr,
    },
    EnumStore {
        local: LirLocalId,
        enum_ty: &'a Type,
        class: TypeId,
        variant: &'a str,
        index: usize,
        child: &'a HirExpr,
    },
    Raise {
        node: &'a HirExpr,
        message: Option<&'a HirExpr>,
    },
    FormatNext {
        node: &'a HirExpr,
        template: crate::interpolate::CheckedTemplate<'a>,
        operands: &'a [HirExpr],
    },
    FormatValue {
        node: &'a HirExpr,
        child: &'a HirExpr,
        format: Option<crate::interpolate::F64Format>,
    },
    ArrayStore {
        array: LirLocalId,
        index: usize,
        element: &'a Type,
        child: &'a HirExpr,
    },
}

enum Finish<'a> {
    Await(&'a HirExpr),
    StaticCall {
        class: TypeId,
        method: &'a str,
        args: &'a [HirExpr],
    },
    New {
        class: TypeId,
        args: &'a [HirExpr],
    },
    Index {
        array: &'a HirExpr,
        index: &'a HirExpr,
    },
    Field {
        object: &'a HirExpr,
        field: &'a str,
    },
    Range {
        start: &'a HirExpr,
        end: &'a HirExpr,
    },
    Print {
        value: &'a HirExpr,
        newline: bool,
    },
    Method(MethodOperands<'a>, Option<BuiltinMethod>),
    LocalCall {
        name: FunctionId,
        snapshot: LirLocalId,
        args: &'a [HirExpr],
    },
    Invoke {
        params: &'a [Type],
        name: FunctionId,
        callee: &'a HirExpr,
        args: &'a [HirExpr],
    },
    Call {
        callee: FunctionId,
        args: &'a [HirExpr],
    },
    Unary {
        op: &'a UnaryOp,
        operand: &'a HirExpr,
    },
    Binary {
        op: &'a BinOp,
        lhs: &'a HirExpr,
        rhs: &'a HirExpr,
    },
}

fn push_typed_arguments<'e, 'p>(
    work: &mut Vec<Work<'e>>,
    args: &'e [HirExpr],
    params: &'p [Type],
    builtin_panic: Option<FunctionId>,
    coercion: impl Fn(&'e HirExpr, usize, &'p Type) -> Work<'e>,
) {
    let paired = args.len().min(params.len());
    work.extend(args[paired..].iter().rev().map(Work::Enter));
    for (index, (child, target)) in args.iter().zip(params).enumerate().rev() {
        if !exits_before_conversion(child, builtin_panic)
            && child.ty != *target
            && !matches!(child.kind, HirExprKind::ReferenceArg { .. })
        {
            work.push(coercion(child, index, target));
        }
        work.push(Work::Enter(child));
    }
}

fn callable_params(ty: &Type) -> &[Type] {
    let (Type::Fn(params, _) | Type::Closure(params, _)) = ty else {
        unreachable!("a validated indirect call has a function or closure signature")
    };
    params
}

/// Per-expression state shared by the iterative scalar lowering actions.
struct ScalarLowering<'a, 'e, 'r> {
    emitted: &'a mut Vec<SourceInst>,
    locals: &'a mut Vec<LirLocal>,
    names: &'a HashMap<String, LirLocalId>,
    callables: &'e Callables<'e, 'r>,
    is_async: bool,
    snapshots: bool,
    plan: &'a mut ScalarPlan<'e>,
    values: HashMap<*const HirExpr, LirOperand>,
    reference_sites: HashMap<*const HirExpr, (FunctionId, usize)>,
    work: Vec<Work<'e>>,
}

impl<'e> ScalarLowering<'_, 'e, '_> {
    fn emit(&mut self, value: LirRvalue, ty: Type, span: Span) -> LirLocalId {
        emit(value, ty, span, self.emitted, self.locals, self.names)
    }

    fn finish_await(&mut self, node: &HirExpr, inner: &HirExpr) -> LirOperand {
        LirOperand::Local(self.emit(
            LirRvalue::AwaitFuture {
                future: self.values[&std::ptr::from_ref(inner)].clone(),
                result: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_static_call(
        &mut self,
        node: &HirExpr,
        class: TypeId,
        method: &str,
        args: &[HirExpr],
    ) -> LirOperand {
        LirOperand::Local(
            self.emit(
                LirRvalue::StaticCall {
                    class,
                    method: method.to_owned(),
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

    fn finish_new(&mut self, node: &HirExpr, class: TypeId, args: &[HirExpr]) -> LirOperand {
        let object = self.values[&std::ptr::from_ref(node)].clone();
        self.emit(
            LirRvalue::ConstructorCall {
                object: object.clone(),
                class,
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

    fn finish_index(&mut self, node: &HirExpr, array: &HirExpr, index: &HirExpr) -> LirOperand {
        LirOperand::Local(self.emit(
            LirRvalue::Index {
                array: self.values[&std::ptr::from_ref(array)].clone(),
                index: self.values[&std::ptr::from_ref(index)].clone(),
                element: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_field_access(&mut self, node: &HirExpr, object: &HirExpr, field: &str) -> LirOperand {
        LirOperand::Local(self.emit(
            LirRvalue::FieldLoad {
                object: self.values[&std::ptr::from_ref(object)].clone(),
                object_ty: object.ty.clone(),
                field: field.to_owned(),
                result: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_static_field(&mut self, node: &HirExpr, class: &TypeId, field: &str) -> LirOperand {
        let value = if self.callables.resolution.enums.contains_key(class) {
            LirRvalue::EnumAlloc {
                class: *class,
                variant: field.to_owned(),
                enum_ty: node.ty.clone(),
            }
        } else {
            LirRvalue::StaticField {
                class: *class,
                field: field.to_owned(),
                result: node.ty.clone(),
            }
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_range(&mut self, node: &HirExpr, start: &HirExpr, end: &HirExpr) -> LirOperand {
        LirOperand::Local(self.emit(
            LirRvalue::Range {
                start: self.values[&std::ptr::from_ref(start)].clone(),
                end: self.values[&std::ptr::from_ref(end)].clone(),
            },
            node.ty.clone(),
            node.span,
        ))
    }

    fn finish_lambda(&mut self, node: &HirExpr, id: ExprId, captures: &[HirCapture]) -> LirOperand {
        let value = LirRvalue::Closure {
            id,
            captures: captures
                .iter()
                .map(|capture| LirOperand::Local(self.names[&capture.source]))
                .collect(),
            ty: node.ty.clone(),
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_print(&mut self, node: &HirExpr, value: &HirExpr, newline: bool) -> LirOperand {
        let value = LirRvalue::Print {
            value: self.values[&std::ptr::from_ref(value)].clone(),
            ty: value.ty.clone(),
            newline,
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_var(&mut self, node: &HirExpr, name: &str) -> LirOperand {
        let source = LirOperand::Local(self.names[name]);
        if self.snapshots {
            LirOperand::Local(self.emit(LirRvalue::Use(source), node.ty.clone(), node.span))
        } else {
            source
        }
    }

    fn finish_method_call(
        &mut self,
        node: &HirExpr,
        call: MethodOperands<'_>,
        builtin: Option<BuiltinMethod>,
    ) -> LirOperand {
        let receiver = self.values[&std::ptr::from_ref(call.object)].clone();
        let operands = call
            .args
            .iter()
            .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
            .collect();
        let arg_types = call
            .args
            .iter()
            .map(|arg| {
                self.values[&std::ptr::from_ref(arg)]
                    .ty(self.locals)
                    .expect("typed argument")
            })
            .collect();
        let value = match builtin {
            Some(BuiltinMethod::Intrinsic(id)) => LirRvalue::IntrinsicCall {
                intrinsic: id,
                method: call.method.clone(),
                receiver,
                receiver_ty: call.object.ty.clone(),
                args: operands,
                arg_types,
                result: node.ty.clone(),
            },
            Some(BuiltinMethod::Adt(id)) => LirRvalue::EnumMethod {
                receiver,
                receiver_ty: call.object.ty.clone(),
                method: id,
                args: operands,
                arg_types,
                result: node.ty.clone(),
            },
            None => LirRvalue::MethodCall {
                receiver,
                receiver_ty: call.object.ty.clone(),
                method: call.method.to_string(),
                // Filled in the common final call walk below.
                is_async: false,
                args: operands,
                arg_types,
                result: node.ty.clone(),
                super_class: call.super_class,
            },
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_invoke(
        &mut self,
        node: &HirExpr,
        name: FunctionId,
        callee: LirOperand,
        args: &[HirExpr],
        params: Vec<Type>,
    ) -> LirOperand {
        let value = LirRvalue::IndirectCall {
            callee,
            name,
            args: args
                .iter()
                .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
                .collect(),
            params,
            result: node.ty.clone(),
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_call(&mut self, node: &HirExpr, callee: FunctionId, args: &[HirExpr]) -> LirOperand {
        let operands = args
            .iter()
            .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
            .collect();
        let value = if let Some(signature) = self.callables.signatures.get(&callee) {
            let params = signature.params.iter().cloned().collect();
            if signature.is_async {
                LirRvalue::StartTask {
                    callee,
                    args: operands,
                    params,
                    output: signature.output.clone(),
                }
            } else {
                LirRvalue::DirectCall {
                    callee,
                    args: operands,
                    params,
                    result: signature.output.clone(),
                }
            }
        } else if callee.is_free_named("recover") {
            LirRvalue::Recover
        } else {
            LirRvalue::BuiltinCall {
                callee,
                args: operands,
                params: args.iter().map(|arg| arg.ty.clone()).collect(),
                result: node.ty.clone(),
            }
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_unary(&mut self, node: &HirExpr, op: &UnaryOp, operand: &HirExpr) -> LirOperand {
        let value = LirRvalue::Unary {
            op: op.clone(),
            operand: self.values[&std::ptr::from_ref(operand)].clone(),
            ty: node.ty.clone(),
        };
        LirOperand::Local(self.emit(value, node.ty.clone(), node.span))
    }

    fn finish_binary(
        &mut self,
        node: &HirExpr,
        op: &BinOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> LirOperand {
        let lhs_value = self.values[&std::ptr::from_ref(lhs)].clone();
        let value = LirRvalue::Binary {
            op: op.clone(),
            lhs: lhs_value.clone(),
            rhs: self.values[&std::ptr::from_ref(rhs)].clone(),
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

    fn finish(&mut self, node: &HirExpr, action: Finish<'e>) {
        let operand = match action {
            Finish::Await(inner) => self.finish_await(node, inner),
            Finish::StaticCall {
                class,
                method,
                args,
            } => self.finish_static_call(node, class, method, args),
            Finish::New { class, args } => self.finish_new(node, class, args),
            Finish::Index { array, index } => self.finish_index(node, array, index),
            Finish::Field { object, field } => self.finish_field_access(node, object, field),
            Finish::Range { start, end } => self.finish_range(node, start, end),
            Finish::Print { value, newline } => self.finish_print(node, value, newline),
            Finish::Method(call, builtin) => self.finish_method_call(node, call, builtin),
            Finish::Invoke {
                name,
                callee,
                args,
                params,
            } => self.finish_invoke(
                node,
                name,
                self.values[&std::ptr::from_ref(callee)].clone(),
                args,
                params.to_vec(),
            ),
            Finish::LocalCall {
                name,
                snapshot,
                args,
            } => {
                let params = callable_params(&self.locals[snapshot.0 as usize].ty).to_vec();
                self.finish_invoke(node, name, LirOperand::Local(snapshot), args, params)
            }
            Finish::Call { callee, args } => self.finish_call(node, callee, args),
            Finish::Unary { op, operand } => self.finish_unary(node, op, operand),
            Finish::Binary { op, lhs, rhs } => self.finish_binary(node, op, lhs, rhs),
        };
        self.values.insert(std::ptr::from_ref(node), operand);
    }

    fn enter_reference_arg(&mut self, node: &'e HirExpr, place: &'e HirExpr) {
        let captured = match &place.kind {
            HirExprKind::Var(name) => LirPlace::Local(self.names[name]),
            HirExprKind::FieldAccess { object, field } => {
                let local = reserve(object.ty.clone(), self.locals, self.names);
                self.work.push(Work::CaptureField { object, local });
                self.work.push(Work::Enter(object));
                LirPlace::Field {
                    object: local,
                    object_ty: object.ty.clone(),
                    field: field.clone(),
                    ty: place.ty.clone(),
                }
            }
            HirExprKind::Index { array, index } => {
                let owner = reserve(Type::Void, self.locals, self.names);
                self.locals[owner.0 as usize].storage_kind =
                    super::super::super::LirStorageKind::GcOwner;
                let slot = reserve(Type::I64, self.locals, self.names);
                self.work.push(Work::CaptureElement {
                    array,
                    index,
                    owner,
                    slot,
                    span: &place.span,
                });
                self.work.push(Work::Enter(index));
                self.work.push(Work::Enter(array));
                LirPlace::ArrayElement {
                    owner,
                    index: slot,
                    element: place.ty.clone(),
                }
            }
            ir_variants!(HirExprKind; Int, Float, Bool, Str, FnRef, Binary, Unary, Call, Invoke, Print, Array, Ternary,
                New, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg, Range, Await, TryPropagate, Lambda, Match, Select) =>
            {
                unreachable!("a validated reference place is a variable, field or array element")
            }
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
    }

    fn enter_enum(
        &mut self,
        node: &'e HirExpr,
        class: TypeId,
        method: &'e str,
        args: &'e [HirExpr],
    ) {
        let local = self.emit(
            LirRvalue::EnumAlloc {
                class,
                variant: method.to_owned(),
                enum_ty: node.ty.clone(),
            },
            node.ty.clone(),
            node.span,
        );
        self.values
            .insert(std::ptr::from_ref(node), LirOperand::Local(local));
        for (index, child) in args.iter().enumerate().rev() {
            self.work.push(Work::EnumStore {
                enum_ty: &node.ty,
                local,
                class,
                variant: method,
                index,
                child,
            });
            self.work.push(Work::Enter(child));
        }
    }

    fn enter_new(&mut self, node: &'e HirExpr, class: &TypeId, args: &'e [HirExpr]) {
        let object = self.emit(
            LirRvalue::ObjectAlloc { class: *class },
            node.ty.clone(),
            node.span,
        );
        self.values
            .insert(std::ptr::from_ref(node), LirOperand::Local(object));
        if self.callables.resolution.classes[class]
            .constructor
            .is_none()
        {
            let fields = self
                .callables
                .resolution
                .fields_rev(class)
                .expect("scalar preflight resolved the memberwise field layout");
            for ((field, _), child) in fields.zip(args.iter().rev()) {
                self.work.push(Work::FieldStore {
                    object,
                    object_ty: &node.ty,
                    field,
                    child,
                });
                self.work.push(Work::Enter(child));
            }
        } else {
            if args
                .iter()
                .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
            {
                self.emit(LirRvalue::BeginReferenceCall, Type::Void, node.span);
            }
            self.work.push(Work::Finish(
                node,
                Finish::New {
                    class: *class,
                    args,
                },
            ));
            self.push_validated_arguments(node, args);
        }
    }

    fn enter_object_literal(
        &mut self,
        node: &'e HirExpr,
        class: TypeId,
        fields: &'e [(String, HirExpr)],
    ) {
        let object = self.emit(LirRvalue::ObjectAlloc { class }, node.ty.clone(), node.span);
        self.values
            .insert(std::ptr::from_ref(node), LirOperand::Local(object));
        for (field, child) in fields.iter().rev() {
            self.work.push(Work::FieldStore {
                object,
                object_ty: &node.ty,
                field,
                child,
            });
            self.work.push(Work::Enter(child));
        }
    }

    fn prepare_references(
        &mut self,
        node: &HirExpr,
        args: &[HirExpr],
        begin: bool,
        callee: impl FnOnce() -> Option<FunctionId>,
    ) {
        let mut sites = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
            .peekable();
        if sites.peek().is_none() {
            return;
        }
        let Some(callee) = callee() else { return };
        for (index, arg) in sites {
            self.reference_sites
                .insert(std::ptr::from_ref(arg), (callee, index));
        }
        if begin {
            self.emit(LirRvalue::BeginReferenceCall, Type::Void, node.span);
        }
    }

    fn enter_call(
        &mut self,
        node: &'e HirExpr,
        callee: FunctionId,
        is_local: bool,
        args: &'e [HirExpr],
    ) {
        self.prepare_references(node, args, true, || Some(callee));
        if is_local {
            let source = self.plan.local_callees[&callee];
            let ty = self.locals[source.0 as usize].ty.clone();
            let snapshot = self.emit(LirRvalue::Use(LirOperand::Local(source)), ty, node.span);
            self.work.push(Work::Finish(
                node,
                Finish::LocalCall {
                    name: callee,
                    snapshot,
                    args,
                },
            ));
            let params = callable_params(&self.locals[snapshot.0 as usize].ty);
            push_typed_arguments(
                &mut self.work,
                args,
                params,
                self.plan.builtin_panic,
                |child, index, _| Work::CoerceSnapshot {
                    child,
                    snapshot,
                    index,
                },
            );
            return;
        }
        let panic = Some(callee) == self.plan.builtin_panic;
        if panic {
            let message = if args.len() == 1 { args.first() } else { None };
            self.work.push(Work::Raise { node, message });
            if args.is_empty() {
                self.format_literal(node, "explicit panic".to_owned());
                return;
            }
            if let Some(message) = message {
                self.work.push(Work::Enter(message));
                return;
            }
        }
        if (callee.is_free_named("format") || panic)
            && !self.callables.signatures.contains_key(&callee)
        {
            let template = self
                .plan
                .formats
                .as_mut()
                .and_then(|formats| formats.remove(&std::ptr::from_ref(node)))
                .expect("scalar preflight checked the format template");
            self.next_format_piece(node, template, &args[1..]);
            return;
        }

        self.work
            .push(Work::Finish(node, Finish::Call { callee, args }));
        self.push_validated_arguments(node, args);
    }

    fn next_format_piece(
        &mut self,
        node: &'e HirExpr,
        mut template: crate::interpolate::CheckedTemplate<'e>,
        operands: &'e [HirExpr],
    ) {
        let (literal, operand) = template.render_piece();
        if !literal.is_empty() || operands.is_empty() {
            self.format_literal(node, literal);
        }
        if let Some(operand) = operand {
            let format = match operand {
                crate::interpolate::SourceFormat::Display => None,
                crate::interpolate::SourceFormat::Precision(format) => Some(format),
            };
            let (child, rest) = operands
                .split_first()
                .expect("a checked placeholder has an operand");
            if !template.as_str().is_empty() {
                self.work.push(Work::FormatNext {
                    node,
                    template,
                    operands: rest,
                });
            }
            self.work.push(Work::FormatValue {
                node,
                child,
                format,
            });
            self.work.push(Work::Enter(child));
        }
    }

    fn enter_array(&mut self, node: &'e HirExpr, elements: &'e [HirExpr]) {
        let Type::Array(element) = &node.ty else {
            unreachable!("a validated array literal has array type")
        };
        let array = self.emit(
            LirRvalue::ArrayAlloc {
                length: elements.len(),
                element: (**element).clone(),
            },
            node.ty.clone(),
            node.span,
        );
        self.values
            .insert(std::ptr::from_ref(node), LirOperand::Local(array));
        for (index, child) in elements.iter().enumerate().rev() {
            self.work.push(Work::ArrayStore {
                array,
                index,
                element,
                child,
            });
            self.work.push(Work::Enter(child));
        }
    }

    fn push_validated_arguments(&mut self, node: &'e HirExpr, args: &'e [HirExpr]) {
        let Some(coercions) = self.plan.coercions.as_mut() else {
            self.work.extend(args.iter().rev().map(Work::Enter));
            return;
        };
        for (index, child) in args.iter().enumerate().rev() {
            if let Some(parameter) = coercions.remove(&std::ptr::from_ref(child)) {
                self.work.push(match parameter {
                    Parameter::Declared(target) => Work::Coerce { child, target },
                    Parameter::ResultArgument => Work::Coerce {
                        child,
                        target: Cow::Borrowed(&node.ty.child_parts().0[index]),
                    },
                    Parameter::Projected(target) => Work::CoerceProjected { child, target },
                });
            }
            self.work.push(Work::Enter(child));
        }
    }

    fn enter_method(
        &mut self,
        node: &'e HirExpr,
        call: MethodOperands<'e>,
        builtin: Option<BuiltinMethod>,
    ) {
        self.work
            .push(Work::Finish(node, Finish::Method(call, builtin)));
        if builtin.is_some() {
            self.work.extend(call.args.iter().rev().map(Work::Enter));
        } else {
            self.prepare_references(node, call.args, false, || {
                let (Type::Named(owner) | Type::Generic(owner, _)) = &call.object.ty else {
                    return None;
                };
                Some(FunctionId::method(
                    call.super_class.unwrap_or(*owner),
                    call.method,
                ))
            });
            self.push_validated_arguments(node, call.args);
            self.work.push(Work::PrepareMethod(node, call));
        }
        self.work.push(Work::Enter(call.object));
    }

    fn enter_await(&mut self, node: &'e HirExpr, inner: &'e HirExpr) {
        if !self.is_async
            && let HirExprKind::Call {
                callee,
                args,
                binding: HirCallBinding::Function,
            } = &inner.kind
            && (callee.is_free_named("sleep") || callee.is_free_named("yield"))
            && !self.callables.signatures.contains_key(callee)
        {
            self.work.push(Work::AwaitBuiltin {
                node,
                call_span: &inner.span,
                callee: *callee,
                args,
            });
            self.work.extend(args.iter().rev().map(Work::Enter));
        } else {
            self.work.push(Work::Finish(node, Finish::Await(inner)));
            self.work.push(Work::Enter(inner));
        }
    }

    fn enter_binary(
        &mut self,
        node: &'e HirExpr,
        op: &'e BinOp,
        lhs: &'e HirExpr,
        rhs: &'e HirExpr,
    ) {
        self.work
            .push(Work::Finish(node, Finish::Binary { op, lhs, rhs }));
        let target = self
            .plan
            .identities
            .as_mut()
            .and_then(|targets| targets.remove(&std::ptr::from_ref(node)));
        if matches!(target, Some(IdentitySide::Left)) {
            self.work.push(Work::Coerce {
                child: rhs,
                target: Cow::Borrowed(&lhs.ty),
            });
        }
        self.work.push(Work::Enter(rhs));
        if matches!(target, Some(IdentitySide::Right)) {
            self.work.push(Work::Coerce {
                child: lhs,
                target: Cow::Borrowed(&rhs.ty),
            });
        }
        self.work.push(Work::Enter(lhs));
    }

    fn enter(&mut self, node: &'e HirExpr) -> Option<LirLocalId> {
        match &node.kind {
            HirExprKind::Call {
                callee,
                binding,
                args,
            } => self.enter_call(node, *callee, binding.is_local(), args),
            HirExprKind::New { class, args } => {
                self.prepare_references(node, args, false, || {
                    Some(FunctionId::method(*class, "init"))
                });
                self.enter_new(node, class, args);
            }
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } => {
                self.prepare_references(node, args, true, || {
                    Some(FunctionId::method(*class, method))
                });
                if self.callables.resolution.enums.contains_key(class) {
                    self.enter_enum(node, *class, method, args);
                } else {
                    self.work.push(Work::Finish(
                        node,
                        Finish::StaticCall {
                            class: *class,
                            method,
                            args,
                        },
                    ));
                    self.push_validated_arguments(node, args);
                }
            }
            HirExprKind::Invoke { name, callee, args } => {
                let params = callable_params(&callee.ty);
                self.work.push(Work::Finish(
                    node,
                    Finish::Invoke {
                        params,
                        name: *name,
                        callee,
                        args,
                    },
                ));
                push_typed_arguments(
                    &mut self.work,
                    args,
                    params,
                    self.plan.builtin_panic,
                    |child, _, ty| Work::Coerce {
                        child,
                        target: Cow::Borrowed(ty),
                    },
                );
                self.work.push(Work::Enter(callee));
            }
            HirExprKind::MethodCall {
                object,
                method,
                args,
                super_class,
                builtin,
            } => self.enter_method(
                node,
                MethodOperands {
                    object,
                    method,
                    args,
                    super_class: *super_class,
                },
                *builtin,
            ),
            HirExprKind::Await { inner } => self.enter_await(node, inner),
            HirExprKind::ReferenceArg { place } => self.enter_reference_arg(node, place),
            HirExprKind::Array { elements } => self.enter_array(node, elements),
            HirExprKind::ObjectLiteral { class, fields } => {
                self.enter_object_literal(node, *class, fields)
            }
            HirExprKind::Unary { op, operand } => {
                self.work
                    .push(Work::Finish(node, Finish::Unary { op, operand }));
                self.work.push(Work::Enter(operand));
            }
            HirExprKind::Binary { op, lhs, rhs } => self.enter_binary(node, op, lhs, rhs),
            HirExprKind::Print { value, newline } => {
                self.work.push(Work::Finish(
                    node,
                    Finish::Print {
                        value,
                        newline: *newline,
                    },
                ));
                self.work.push(Work::Enter(value));
            }
            HirExprKind::Index { array, index } => {
                self.work
                    .push(Work::Finish(node, Finish::Index { array, index }));
                self.work.push(Work::Enter(index));
                self.work.push(Work::Enter(array));
            }
            HirExprKind::FieldAccess { object, field } => {
                self.work
                    .push(Work::Finish(node, Finish::Field { object, field }));
                self.work.push(Work::Enter(object));
            }
            HirExprKind::Range { start, end } => {
                self.work
                    .push(Work::Finish(node, Finish::Range { start, end }));
                self.work.push(Work::Enter(end));
                self.work.push(Work::Enter(start));
            }
            // Leaves have no pending children. Their result is ready immediately.
            HirExprKind::Int(value) => {
                self.values
                    .insert(node, LirOperand::integer(*value, &node.ty));
            }
            HirExprKind::Float(value) => {
                self.values.insert(node, LirOperand::Float(*value));
            }
            HirExprKind::Bool(value) => {
                self.values.insert(node, LirOperand::Bool(*value));
            }
            HirExprKind::Var(name) => {
                if node.ty == Type::Never {
                    return Some(self.names[name]);
                }
                let operand = self.finish_var(node, name);
                self.values.insert(node, operand);
            }
            HirExprKind::Str(value) => {
                let local = self.emit(
                    LirRvalue::StringLiteral(value.clone()),
                    node.ty.clone(),
                    node.span,
                );
                self.values.insert(node, LirOperand::Local(local));
            }
            HirExprKind::FnRef(function) => {
                let local = self.emit(
                    LirRvalue::FunctionRef {
                        function: *function,
                        ty: node.ty.clone(),
                    },
                    node.ty.clone(),
                    node.span,
                );
                self.values.insert(node, LirOperand::Local(local));
            }
            HirExprKind::Lambda { id, captures, .. } => {
                let operand = self.finish_lambda(node, *id, captures);
                self.values.insert(node, operand);
            }
            HirExprKind::StaticField { class, field } => {
                let operand = self.finish_static_field(node, class, field);
                self.values.insert(node, operand);
            }
            HirExprKind::Ternary { .. }
            | HirExprKind::TryPropagate { .. }
            | HirExprKind::Match { .. }
            | HirExprKind::Select { .. } => {
                unreachable!("scalar validation excludes control-flow expressions")
            }
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

    fn await_builtin(
        &mut self,
        node: &'e HirExpr,
        call_span: Span,
        callee: FunctionId,
        args: &[HirExpr],
    ) {
        let future_type = Type::Generic(TypeId::local("Future"), vec![Type::Void]);
        let future = self.emit(
            LirRvalue::BuiltinCall {
                callee,
                args: args
                    .iter()
                    .map(|arg| self.values[&std::ptr::from_ref(arg)].clone())
                    .collect(),
                params: args.iter().map(|arg| arg.ty.clone()).collect(),
                result: future_type.clone(),
            },
            future_type,
            call_span,
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

    fn capture_element(
        &mut self,
        array: &HirExpr,
        index: &HirExpr,
        owner: LirLocalId,
        slot: LirLocalId,
        span: Span,
    ) {
        push_compute(
            self.emitted,
            slot,
            LirRvalue::Use(self.values[&std::ptr::from_ref(index)].clone()),
            index.span,
        );
        push_compute(
            self.emitted,
            owner,
            LirRvalue::CaptureArrayOwner {
                array: self.values[&std::ptr::from_ref(array)].clone(),
                index: LirOperand::Local(slot),
            },
            span,
        );
    }

    fn prepare_method(&mut self, node: &'e HirExpr, call: MethodOperands<'e>) {
        let receiver = self.values[&std::ptr::from_ref(call.object)].clone();
        let local = self.emit(
            LirRvalue::PrepareMethod {
                receiver,
                receiver_ty: call.object.ty.clone(),
                method: call.method.to_string(),
            },
            call.object.ty.clone(),
            node.span,
        );
        self.values
            .insert(std::ptr::from_ref(call.object), LirOperand::Local(local));
        if call
            .args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.emit(LirRvalue::BeginReferenceCall, Type::Void, node.span);
        }
    }

    fn field_store(
        &mut self,
        object: LirLocalId,
        object_ty: &Type,
        field: &str,
        child: &'e HirExpr,
    ) {
        self.emit(
            LirRvalue::FieldStore {
                object: LirOperand::Local(object),
                object_ty: object_ty.clone(),
                field: field.to_owned(),
                value: self.values[&std::ptr::from_ref(child)].clone(),
            },
            Type::Void,
            child.span,
        );
    }

    fn enum_store(
        &mut self,
        local: LirLocalId,
        enum_ty: &Type,
        class: TypeId,
        variant: &str,
        index: usize,
        child: &'e HirExpr,
    ) {
        push_compute(
            self.emitted,
            local,
            LirRvalue::EnumPayloadStore {
                object: LirOperand::Local(local),
                class,
                variant: variant.to_owned(),
                index,
                value: self.values[&std::ptr::from_ref(child)].clone(),
                source: child.ty.clone(),
                enum_ty: enum_ty.clone(),
            },
            child.span,
        );
    }

    fn raise(&mut self, node: &'e HirExpr, message: Option<&'e HirExpr>) -> Option<LirLocalId> {
        let message = message
            .map(|child| self.values[&std::ptr::from_ref(child)].clone())
            .unwrap_or_else(|| self.values[&std::ptr::from_ref(node)].clone());
        let result = self.emit(LirRvalue::Panic { message }, Type::Never, node.span);
        // No enclosing operand, coercion, store, or later argument can
        // execute after this raise. Preserve only the evaluated prefix.
        Some(result)
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

    fn array_store(&mut self, array: LirLocalId, index: usize, element: &Type, child: &'e HirExpr) {
        self.emit(
            LirRvalue::ArrayStore {
                array: LirOperand::Local(array),
                index: LirOperand::Int(index as i64),
                value: self.values[&std::ptr::from_ref(child)].clone(),
                element: element.clone(),
            },
            Type::Void,
            child.span,
        );
    }

    // A divergent action returns the replacement root and stops the worklist.
    // Ordinary actions leave the root untouched until all operands are ready.
    fn step(&mut self, action: Work<'e>) -> Option<LirLocalId> {
        match action {
            Work::Coerce { child, target } => {
                self.coerce(child, target.into_owned());
            }
            Work::CoerceProjected { child, target } => {
                let ty = target.bindings.specialize(target.param);
                #[cfg(test)]
                {
                    let nodes = tests::type_nodes(&ty);
                    PEAK_MATERIALIZED_TARGET_NODES
                        .set(PEAK_MATERIALIZED_TARGET_NODES.get().max(nodes));
                }
                self.coerce(child, ty);
            }
            Work::CoerceSnapshot {
                child,
                snapshot,
                index,
            } => {
                let target = callable_params(&self.locals[snapshot.0 as usize].ty)[index].clone();
                self.coerce(child, target);
            }
            Work::AwaitBuiltin {
                node,
                call_span,
                callee,
                args,
            } => {
                self.await_builtin(node, *call_span, callee, args);
            }
            Work::CaptureField { object, local } => {
                push_compute(
                    self.emitted,
                    local,
                    LirRvalue::Use(self.values[&std::ptr::from_ref(object)].clone()),
                    object.span,
                );
            }
            Work::CaptureElement {
                array,
                index,
                owner,
                slot,
                span,
            } => {
                self.capture_element(array, index, owner, slot, *span);
            }
            Work::PrepareMethod(node, call) => {
                self.prepare_method(node, call);
            }
            Work::FieldStore {
                object,
                object_ty,
                field,
                child,
            } => {
                self.field_store(object, object_ty, field, child);
            }
            Work::EnumStore {
                local,
                enum_ty,
                class,
                variant,
                index,
                child,
            } => {
                self.enum_store(local, enum_ty, class, variant, index, child);
            }
            Work::Raise { node, message } => return self.raise(node, message),
            Work::FormatNext {
                node,
                template,
                operands,
            } => {
                self.next_format_piece(node, template, operands);
            }
            Work::FormatValue {
                node,
                child,
                format,
            } => {
                self.format_value(node, child, format);
            }
            Work::ArrayStore {
                array,
                index,
                element,
                child,
            } => {
                self.array_store(array, index, element, child);
            }
            Work::Finish(node, action) => {
                self.finish(node, action);
            }
            Work::Enter(node) => return self.enter(node),
        }
        None
    }
}

#[cfg(test)]
mod tests;
