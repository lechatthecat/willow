use super::*;
use super::LirPlace;

#[derive(Default)]
struct Callables {
    signatures: HashMap<FunctionId, (Vec<Type>, Type)>,
    async_outputs: HashMap<FunctionId, Type>,
    resolution: crate::ir::typed_ast::HirResolution,
}
impl std::ops::Deref for Callables {
    type Target = HashMap<FunctionId, (Vec<Type>, Type)>;
    fn deref(&self) -> &Self::Target { &self.signatures }
}

fn class_fields(class: &TypeId, callables: &Callables) -> Option<Vec<(String, Type)>> {
    super::super::class_fields(&callables.resolution, class)
}

fn static_call(class: &TypeId, method: &str, args: &[HirExpr], result: &Type, callables: &Callables) -> bool {
    if let Some(namespace) = callables.resolution.namespaces.get(class) {
        if let Some(signature) = crate::semantic::intrinsics::namespace_builtin(namespace, method) {
            return signature.ret == *result && signature.params.len() == args.len() && signature.params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, &callables.resolution));
        }
    }
    if let Some(signature) = super::super::method_signature(&callables.resolution, &Type::Named(*class), method) {
        return signature.is_static && !signature.is_async && signature.return_type == *result && signature.params.len() == args.len() && signature.params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, &callables.resolution));
    }
    if callables.resolution.classes.contains_key(class) || callables.resolution.enums.contains_key(class) { return false; }
    let class_name = class.to_string();
    match (class_name.as_str(), method, args) {
        ("Map" | "Channel" | "CancelToken" | "CancelSource", "new", []) => true,
        ("Channel", "with_capacity", [arg]) => arg.ty == Type::I64,
        ("AtomicI64" | "AtomicBool" | "CellI64" | "CellBool" | "Mutex" | "RwLock", "new", [arg]) => {
            use crate::semantic::builtin_types;
            builtin_types::resolve(result).is_some_and(|resolved| resolved.args.first().is_some_and(|ty| argument_coercible(ty, arg, &callables.resolution)))
                || matches!((class_name.as_str(), &arg.ty), ("AtomicI64" | "CellI64", Type::I64) | ("AtomicBool" | "CellBool", Type::Bool))
        }
        _ => false,
    }
}

fn constructor_supported(class: &TypeId, args: &[HirExpr], callables: &Callables) -> bool {
    let Some(info) = callables.resolution.classes.get(class) else { return false; };
    if let Some(signature) = &info.constructor {
        signature.params.len() == args.len() && signature.params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, &callables.resolution))
    } else { class_fields(class, callables).is_some_and(|fields| fields.len() == args.len()) }
}

fn format_segments(args: &[HirExpr]) -> Option<Vec<crate::interpolate::Segment>> {
    super::super::format_segments(args)
}

fn builtin_call(callee: &FunctionId, args: &[HirExpr], result: &Type, is_async: bool) -> bool {
    let name = callee.unqualified_name();
    if !callee.is_free_named(name) { return false; }
    if name == "panic" { return *result == Type::Never && match args { [] => true, [arg] => arg.ty == Type::String, _ => format_segments(args).is_some() }; }
    if name == "format" { return *result == Type::String && format_segments(args).is_some(); }
    if name == "recover" { return args.is_empty(); }
    if name == "sleep" { return !is_async && matches!(args, [arg] if arg.ty == Type::I64) && (*result == Type::Void || *result == Type::Generic(TypeId::local("Future"), vec![Type::Void])); }
    if name == "yield" { return !is_async && args.is_empty() && (*result == Type::Void || *result == Type::Generic(TypeId::local("Future"), vec![Type::Void])); }
    crate::semantic::intrinsics::builtin_call_runtime_name(name).is_some() && args.is_empty()
        && if matches!(name, "gc_collect" | "gc_minor_collect") { *result == Type::Void } else { *result == Type::I64 }
}

fn instance_method(receiver: &Type, method: &str, args: &[HirExpr], callables: &Callables) -> bool {
    super::super::method_signature(&callables.resolution, receiver, method).is_some_and(|signature| !signature.is_static && !signature.is_async
        && signature.params.len() == args.len() && signature.params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, &callables.resolution)))
}

fn enum_method(receiver: &Type, method: &str) -> bool {
    use crate::semantic::builtin_types::{self, BuiltinTypeId};
    builtin_types::resolve(receiver).is_some_and(|resolved| match resolved.id {
        BuiltinTypeId::Option => matches!(method, "is_some" | "is_none" | "unwrap" | "expect" | "unwrap_or" | "map" | "and_then" | "or_else" | "ok_or" | "ok_or_else"),
        BuiltinTypeId::Result => matches!(method, "is_ok" | "is_err" | "unwrap" | "expect" | "unwrap_or" | "unwrap_err" | "expect_err" | "map" | "map_err" | "and_then" | "or_else" | "ok" | "err"),
        _ => false,
    })
}

fn argument_types(node: &HirExpr, names: &HashMap<String, LirLocalId>, locals: &[LirLocal], callables: &Callables) -> Option<Vec<Type>> {
    match &node.kind {
        HirExprKind::Call { callee, .. } => {
            if let Some(local) = names.get(callee.unqualified_name()) {
                match &locals[local.0 as usize].ty { Type::Fn(params, _) | Type::Closure(params, _) => Some(params.clone()), _ => None }
            } else { callables.get(callee).map(|(params, _)| params.clone()) }
        }
        HirExprKind::New { class, .. } => callables.resolution.classes.get(class)?.constructor.as_ref().map(|signature| signature.params.clone()),
        HirExprKind::MethodCall { object, method, .. } => super::super::method_signature(&callables.resolution, &object.ty, method).map(|signature| signature.params),
        HirExprKind::StaticCall { class, method, .. } => {
            if let Some(namespace) = callables.resolution.namespaces.get(class) {
                if let Some(signature) = crate::semantic::intrinsics::namespace_builtin(namespace, method) { return Some(signature.params); }
            }
            if let Some(signature) = super::super::method_signature(&callables.resolution, &Type::Named(*class), method) { return Some(signature.params); }
            if method == "new" && matches!(class.name(), "Mutex" | "RwLock" | "BlockingCell" | "BlockingRwCell") {
                if let Type::Generic(_, params) = &node.ty { return Some(params.clone()); }
            }
            None
        }
        _ => None,
    }
}

fn argument_coercible(target: &Type, arg: &HirExpr, resolution: &crate::ir::typed_ast::HirResolution) -> bool {
    if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) { *target == arg.ty }
    else { resolution.can_coerce(&arg.ty, target) }
}

fn scalar(expr: &HirExpr, names: &HashMap<String, LirLocalId>, locals: &[LirLocal], callables: &Callables, is_async: bool) -> bool {
    expr.walk_postorder(false).all(|node| match &node.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) | HirExprKind::Str(_) | HirExprKind::FnRef(_) => true,
        HirExprKind::ReferenceArg { place } => matches!(place.kind, HirExprKind::Var(_) | HirExprKind::FieldAccess { .. } | HirExprKind::Index { .. }),
        HirExprKind::New { class, args } => constructor_supported(class, args, callables),
        HirExprKind::ObjectLiteral { class, fields } => class_fields(class, callables).is_some_and(|declared| declared.len() == fields.len() && declared.iter().all(|(name, _)| fields.iter().filter(|(field, _)| field == name).count() == 1)),
        HirExprKind::StaticField { class, field } => callables.resolution.enums.get(class).is_some_and(|info| info.variants.iter().any(|variant| variant.name == *field && variant.payloads.is_empty()))
            || callables.resolution.classes.get(class).is_some_and(|info| info.static_fields.contains_key(field)),
        HirExprKind::StaticCall { class, method, args } => callables.resolution.enums.get(class).is_some_and(|info| info.variants.iter().any(|variant| variant.name == *method && variant.payloads.len() == args.len())) || static_call(class, method, args, &node.ty, callables),
        HirExprKind::Await { inner } => crate::semantic::builtin_types::unary_arg(&inner.ty, crate::semantic::builtin_types::BuiltinTypeId::Future) == Some(&node.ty)
            || (!is_async && node.ty == Type::Void && matches!(&inner.kind, HirExprKind::Call { callee, args } if (callee.is_free_named("sleep") || callee.is_free_named("yield")) && builtin_call(callee, args, &inner.ty, is_async))),
        HirExprKind::Array { .. } => matches!(node.ty, Type::Array(_)),
        HirExprKind::Index { .. } | HirExprKind::FieldAccess { .. } | HirExprKind::Range { .. } => true,
        HirExprKind::Unary { .. } => matches!(node.ty, Type::I64 | Type::F64 | Type::Bool),
        HirExprKind::Var(name) => names.contains_key(name),
        HirExprKind::Lambda { captures, .. } => captures.iter().all(|capture| names.contains_key(&capture.source)),
        HirExprKind::Print { value, .. } => matches!(value.ty, Type::I64 | Type::F64 | Type::Bool | Type::String),
        HirExprKind::MethodCall { object, method, args } => crate::semantic::intrinsics::resolve(&object.ty, method, args.len()).is_some_and(|resolved|
            !(is_async && resolved.intrinsic.is_suspension_point()) && resolved.return_type(|i| args.get(i).map(|arg| arg.ty.clone())) == node.ty) || enum_method(&object.ty, method) || instance_method(&object.ty, method, args, callables),
        HirExprKind::Call { callee, args } => if let Some(local) = names.get(callee.unqualified_name()) {
            matches!(&locals[local.0 as usize].ty, Type::Fn(params, result) | Type::Closure(params, result) if !args.iter().any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. })) && **result == node.ty && params.len() == args.len() && params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, &callables.resolution)))
        } else { callables.get(callee).is_some_and(|(params, result)| *result == node.ty
                && params.len() == args.len() && params.iter().zip(args).all(|(param, arg)| argument_coercible(param, arg, &callables.resolution)))
                || builtin_call(callee, args, &node.ty, is_async) },
        HirExprKind::Binary { op, lhs, .. } => !matches!(op, BinOp::And | BinOp::Or)
            && matches!(lhs.ty, Type::I64 | Type::F64 | Type::Bool | Type::String),
        _ => false,
    })
}

fn lower(
    expr: &mut HirExpr,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &mut HashMap<String, LirLocalId>,
    callables: &Callables,
    is_async: bool,
) {
    if !matches!(
        expr.kind,
        HirExprKind::Unary { .. } | HirExprKind::Binary { .. } | HirExprKind::Call { .. }
        | HirExprKind::Await { .. } | HirExprKind::New { .. } | HirExprKind::ObjectLiteral { .. } | HirExprKind::StaticField { .. } | HirExprKind::StaticCall { .. }
        | HirExprKind::Array { .. } | HirExprKind::Index { .. } | HirExprKind::FieldAccess { .. } | HirExprKind::Range { .. }
        | HirExprKind::Str(_) | HirExprKind::Print { .. } | HirExprKind::FnRef(_) | HirExprKind::Lambda { .. } | HirExprKind::MethodCall { .. }
    ) || !scalar(expr, names, locals, callables, is_async)
    {
        return;
    }
    let snapshots = expr.walk_postorder(false).any(|node| matches!(node.kind, HirExprKind::Call { .. } | HirExprKind::MethodCall { .. } | HirExprKind::New { .. } | HirExprKind::StaticCall { .. }));
    let mut values: HashMap<*const HirExpr, LirOperand> = HashMap::new();
    let mut reference_sites: HashMap<*const HirExpr, (FunctionId, usize)> = HashMap::new();
    enum Work<'a> { Enter(&'a HirExpr), Coerce { child: &'a HirExpr, target: Type }, Finish(&'a HirExpr), AwaitBuiltin(&'a HirExpr), Reference(&'a HirExpr), PrepareMethod(&'a HirExpr), FieldStore { object: LirOperand, object_ty: Type, field: String, child: &'a HirExpr }, EnumStore { node: &'a HirExpr, class: TypeId, variant: String, index: usize, child: &'a HirExpr }, Raise { node: &'a HirExpr, message: Option<&'a HirExpr> }, FormatLiteral { node: &'a HirExpr, text: String }, FormatValue { node: &'a HirExpr, child: &'a HirExpr, format: Option<crate::interpolate::F64Format> }, ArrayStore { array: LirOperand, index: usize, element: Type, child: &'a HirExpr } }
    fn arguments<'a>(args: &'a [HirExpr], targets: Option<&[Type]>) -> Vec<Work<'a>> {
        let mut work = Vec::new();
        for (index, child) in args.iter().enumerate().rev() {
            if let Some(target) = targets.and_then(|types| types.get(index)) {
                if *target != child.ty && !matches!(child.kind, HirExprKind::ReferenceArg { .. }) {
                    work.push(Work::Coerce { child, target: target.clone() });
                }
            }
            work.push(Work::Enter(child));
        }
        work
    }
    let mut work = vec![Work::Enter(&*expr)];
    while let Some(action) = work.pop() {
        let node = match action {
            Work::Coerce { child, target } => {
                let operand = values[&std::ptr::from_ref(child)].clone();
                let source = operand.ty(locals).expect("typed argument");
                let local = emit(LirRvalue::Coerce { value: operand, source, target: target.clone() }, target, child.span, emitted, locals, names);
                values.insert(std::ptr::from_ref(child), LirOperand::Local(local));
                continue;
            }
            Work::AwaitBuiltin(node) => {
                let HirExprKind::Await { inner } = &node.kind else { unreachable!() };
                let HirExprKind::Call { callee, args } = &inner.kind else { unreachable!() };
                let future_type = Type::Generic(TypeId::local("Future"), vec![Type::Void]);
                let future = emit(LirRvalue::BuiltinCall { callee: *callee, args: args.iter().map(|arg| values[&std::ptr::from_ref(arg)].clone()).collect(), params: args.iter().map(|arg| arg.ty.clone()).collect(), result: future_type.clone() }, future_type, inner.span, emitted, locals, names);
                let result = emit(LirRvalue::AwaitFuture { future: LirOperand::Local(future), result: Type::Void }, Type::Void, node.span, emitted, locals, names);
                values.insert(std::ptr::from_ref(node), LirOperand::Local(result));
                continue;
            }
            Work::Reference(node) => {
                let HirExprKind::ReferenceArg { place } = &node.kind else { unreachable!() };
                let LirOperand::Reference { place: captured, .. } = values[&std::ptr::from_ref(node)].clone() else { unreachable!() };
                match (&place.kind, captured) {
                    (HirExprKind::Var(_), LirPlace::Local(_)) => {},
                    (HirExprKind::FieldAccess { object, .. }, LirPlace::Field { object: local, .. }) => {
                        emitted.push(SourceInst::Compute { local, value: LirRvalue::Use(values[&std::ptr::from_ref(&**object)].clone()), span: object.span });
                    }
                    (HirExprKind::Index { array, index }, LirPlace::ArrayElement { owner, index: local, .. }) => {
                        let index_value = values[&std::ptr::from_ref(&**index)].clone();
                        emitted.push(SourceInst::Compute { local, value: LirRvalue::Use(index_value.clone()), span: index.span });
                        emitted.push(SourceInst::Compute { local: owner, value: LirRvalue::CaptureArrayOwner { array: values[&std::ptr::from_ref(&**array)].clone(), index: LirOperand::Local(local) }, span: place.span });
                    }
                    _ => unreachable!("reserved reference place"),
                }
                continue;
            }
            Work::PrepareMethod(node) => {
                let HirExprKind::MethodCall { object, method, args } = &node.kind else { unreachable!() };
                let receiver = values[&std::ptr::from_ref(&**object)].clone();
                let local = emit(LirRvalue::PrepareMethod { receiver, receiver_ty: object.ty.clone(), method: method.clone() }, object.ty.clone(), node.span, emitted, locals, names);
                values.insert(std::ptr::from_ref(&**object), LirOperand::Local(local));
                if args.iter().any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. })) { emit(LirRvalue::BeginReferenceCall, Type::Void, node.span, emitted, locals, names); }
                continue;
            }
            Work::FieldStore { object, object_ty, field, child } => {
                emit(LirRvalue::FieldStore { object, object_ty, field, value: values[&std::ptr::from_ref(child)].clone() }, Type::Void, child.span, emitted, locals, names);
                continue;
            }
            Work::EnumStore { node, class, variant, index, child } => {
                let object = values[&std::ptr::from_ref(node)].clone();
                let LirOperand::Local(local) = object else { unreachable!() };
                emitted.push(SourceInst::Compute { local, value: LirRvalue::EnumPayloadStore { object, class, variant, index, value: values[&std::ptr::from_ref(child)].clone(), source: child.ty.clone(), enum_ty: node.ty.clone() }, span: child.span });
                continue;
            }
            Work::Raise { node, message } => {
                let message = message.map(|child| values[&std::ptr::from_ref(child)].clone()).unwrap_or_else(|| values[&std::ptr::from_ref(node)].clone());
                let result = emit(LirRvalue::Panic { message }, Type::Never, node.span, emitted, locals, names);
                values.insert(std::ptr::from_ref(node), LirOperand::Local(result));
                continue;
            }
            Work::FormatLiteral { node, text } => {
                let piece = LirOperand::Local(emit(LirRvalue::StringLiteral(text), Type::String, node.span, emitted, locals, names));
                append_format(node, piece, &mut values, emitted, locals, names);
                continue;
            }
            Work::FormatValue { node, child, format } => {
                let piece = LirOperand::Local(emit(LirRvalue::FormatScalar { value: values[&std::ptr::from_ref(child)].clone(), ty: child.ty.clone(), format }, Type::String, child.span, emitted, locals, names));
                append_format(node, piece, &mut values, emitted, locals, names);
                continue;
            }
            Work::ArrayStore { array, index, element, child } => {
                emit(LirRvalue::ArrayStore { array, index: LirOperand::Int(index as i64), value: values[&std::ptr::from_ref(child)].clone(), element }, Type::Void, child.span, emitted, locals, names);
                continue;
            }
            Work::Finish(node) => node,
            Work::Enter(node) => {
                let targets = argument_types(node, names, locals, callables);
                let reference_call = match &node.kind {
                    HirExprKind::Call { callee, args } => Some((*callee, args)),
                    HirExprKind::MethodCall { object, method, args } => match &object.ty { Type::Named(owner) | Type::Generic(owner, _) => Some((FunctionId::method(*owner, method), args)), _ => None },
                    HirExprKind::New { class, args } => Some((FunctionId::method(*class, "init"), args)),
                    HirExprKind::StaticCall { class, method, args } => Some((FunctionId::method(*class, method), args)),
                    _ => None,
                };
                if let Some((callee, args)) = reference_call {
                    let has_refs = args.iter().enumerate().any(|(_, arg)| matches!(arg.kind, HirExprKind::ReferenceArg { .. }));
                    for (index, arg) in args.iter().enumerate() {
                        if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) { reference_sites.insert(std::ptr::from_ref(arg), (callee, index)); }
                    }
                    if has_refs && !matches!(node.kind, HirExprKind::MethodCall { .. } | HirExprKind::New { .. }) {
                        emit(LirRvalue::BeginReferenceCall, Type::Void, node.span, emitted, locals, names);
                    }
                }
                match &node.kind {
                    HirExprKind::Await { inner } if !is_async && matches!(&inner.kind, HirExprKind::Call { callee, .. } if (callee.is_free_named("sleep") || callee.is_free_named("yield")) && !names.contains_key(callee.unqualified_name()) && !callables.contains_key(callee)) => {
                        let HirExprKind::Call { args, .. } = &inner.kind else { unreachable!() };
                        work.push(Work::AwaitBuiltin(node));
                        work.extend(arguments(args, targets.as_deref()));
                        continue;
                    }
                    HirExprKind::ReferenceArg { place } => {
                        let captured = match &place.kind {
                            HirExprKind::Var(name) => LirPlace::Local(names[name]),
                            HirExprKind::FieldAccess { object, field } => LirPlace::Field { object: reserve(object.ty.clone(), locals, names), object_ty: object.ty.clone(), field: field.clone(), ty: place.ty.clone() },
                            HirExprKind::Index { .. } => {
                                let owner = reserve(Type::Void, locals, names);
                                locals[owner.0 as usize].storage_kind = super::super::LirStorageKind::GcOwner;
                                LirPlace::ArrayElement { owner, index: reserve(Type::I64, locals, names), element: place.ty.clone() }
                            }
                            _ => unreachable!("validated reference place"),
                        };
                        let argument = LirOperand::Reference { place: captured, span: node.span, display: super::super::reference_place_name(place) };
                        if let Some((callee, index)) = reference_sites.get(&std::ptr::from_ref(node)) {
                            emit(LirRvalue::ReferenceDebug { argument: argument.clone(), callee: *callee, index: *index }, Type::Void, node.span, emitted, locals, names);
                        }
                        values.insert(std::ptr::from_ref(node), argument);
                        work.push(Work::Reference(node));
                        match &place.kind {
                            HirExprKind::Var(_) => {},
                            HirExprKind::FieldAccess { object, .. } => work.push(Work::Enter(object)),
                            HirExprKind::Index { array, index } => { work.push(Work::Enter(index)); work.push(Work::Enter(array)); }
                            _ => unreachable!("validated reference place"),
                        }
                        continue;
                    }
                    HirExprKind::MethodCall { object, method, args } if instance_method(&object.ty, method, args, callables) => {
                        work.push(Work::Finish(node));
                        work.extend(arguments(args, targets.as_deref()));
                        work.push(Work::PrepareMethod(node));
                        work.push(Work::Enter(object));
                        continue;
                    }

                    HirExprKind::StaticCall { class, method, args } if callables.resolution.enums.contains_key(class) => {
                        let object = LirOperand::Local(emit(LirRvalue::EnumAlloc { class: *class, variant: method.clone(), enum_ty: node.ty.clone() }, node.ty.clone(), node.span, emitted, locals, names));
                        values.insert(std::ptr::from_ref(node), object);
                        for (index, child) in args.iter().enumerate().rev() {
                            work.push(Work::EnumStore { node, class: *class, variant: method.clone(), index, child });
                            work.push(Work::Enter(child));
                        }
                        continue;
                    }
                    HirExprKind::New { class, args } => {
                        let object = LirOperand::Local(emit(LirRvalue::ObjectAlloc { class: *class }, node.ty.clone(), node.span, emitted, locals, names));
                        values.insert(std::ptr::from_ref(node), object.clone());
                        if callables.resolution.classes[class].constructor.is_some() {
                            if args.iter().any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. })) { emit(LirRvalue::BeginReferenceCall, Type::Void, node.span, emitted, locals, names); }
                            work.push(Work::Finish(node));
                            work.extend(arguments(args, targets.as_deref()));
                        } else {
                            let fields = class_fields(class, callables).expect("validated class");
                            for ((field, _), child) in fields.into_iter().zip(args).rev() {
                                work.push(Work::FieldStore { object: object.clone(), object_ty: node.ty.clone(), field, child });
                                work.push(Work::Enter(child));
                            }
                        }
                        continue;
                    }
                    HirExprKind::ObjectLiteral { class, fields } => {
                        let object = LirOperand::Local(emit(LirRvalue::ObjectAlloc { class: *class }, node.ty.clone(), node.span, emitted, locals, names));
                        values.insert(std::ptr::from_ref(node), object.clone());
                        for (field, child) in fields.iter().rev() {
                            work.push(Work::FieldStore { object: object.clone(), object_ty: node.ty.clone(), field: field.clone(), child });
                            work.push(Work::Enter(child));
                        }
                        continue;
                    }
                    _ => {}
                }
                if let HirExprKind::Call { callee, args } = &node.kind {
                    if let Some(source) = names.get(callee.unqualified_name()).copied() {
                        let ty = locals[source.0 as usize].ty.clone();
                        let snapshot = emit(LirRvalue::Use(LirOperand::Local(source)), ty, node.span, emitted, locals, names);
                        values.insert(std::ptr::from_ref(node), LirOperand::Local(snapshot));
                        work.push(Work::Finish(node));
                        work.extend(arguments(args, targets.as_deref()));
                        continue;
                    }
                    let panic = callee.is_free_named("panic") && !callables.contains_key(callee);
                    if panic {
                        let message = if args.len() == 1 { args.first() } else { None };
                        work.push(Work::Raise { node, message });
                        if args.is_empty() { work.push(Work::FormatLiteral { node, text: "explicit panic".into() }); continue; }
                        if let Some(message) = message { work.push(Work::Enter(message)); continue; }
                    }
                    if (callee.is_free_named("format") || panic) && !callables.contains_key(callee) {
                        let segments = format_segments(args).expect("validated format");
                        let mut actions = Vec::new();
                        let mut operands = args[1..].iter();
                        if segments.is_empty() { actions.push(Work::FormatLiteral { node, text: String::new() }); }
                        for segment in segments {
                            match segment {
                                crate::interpolate::Segment::Literal(text) => actions.push(Work::FormatLiteral { node, text }),
                                segment => {
                                    let child = operands.next().expect("validated placeholder");
                                    let format = match segment { crate::interpolate::Segment::F64(format) => Some(format), _ => None };
                                    actions.push(Work::Enter(child));
                                    actions.push(Work::FormatValue { node, child, format });
                                }
                            }
                        }
                        work.extend(actions.into_iter().rev());
                        continue;
                    }
                }
                if let HirExprKind::Array { elements } = &node.kind {
                    let Type::Array(element) = &node.ty else { unreachable!() };
                    let array = LirOperand::Local(emit(LirRvalue::ArrayAlloc { length: elements.len(), element: (**element).clone() }, node.ty.clone(), node.span, emitted, locals, names));
                    values.insert(std::ptr::from_ref(node), array.clone());
                    for (index, child) in elements.iter().enumerate().rev() {
                        work.push(Work::ArrayStore { array: array.clone(), index, element: (**element).clone(), child });
                        work.push(Work::Enter(child));
                    }
                    continue;
                }
            work.push(Work::Finish(node));
            if let HirExprKind::Call { args, .. } | HirExprKind::StaticCall { args, .. } = &node.kind {
                work.extend(arguments(args, targets.as_deref()));
            } else if !matches!(node.kind, HirExprKind::Lambda { .. }) {
                work.extend(node.children().into_iter().rev().map(Work::Enter));
            }
            continue;
            }
        };
        let operand = match &node.kind {
            HirExprKind::Await { inner } => LirOperand::Local(emit(LirRvalue::AwaitFuture { future: values[&std::ptr::from_ref(&**inner)].clone(), result: node.ty.clone() }, node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::StaticCall { class, method, args } => LirOperand::Local(emit(LirRvalue::StaticCall { class: *class, method: method.clone(), args: args.iter().map(|arg| values[&std::ptr::from_ref(arg)].clone()).collect(), arg_types: args.iter().map(|arg| values[&std::ptr::from_ref(arg)].ty(locals).expect("typed argument")).collect(), result: node.ty.clone() }, node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::New { class, args } => {
                let object = values[&std::ptr::from_ref(node)].clone();
                emit(LirRvalue::ConstructorCall { object: object.clone(), class: *class, args: args.iter().map(|arg| values[&std::ptr::from_ref(arg)].clone()).collect(), arg_types: args.iter().map(|arg| values[&std::ptr::from_ref(arg)].ty(locals).expect("typed argument")).collect() }, Type::Void, node.span, emitted, locals, names);
                object
            }
            HirExprKind::Index { array, index } => LirOperand::Local(emit(LirRvalue::Index { array: values[&std::ptr::from_ref(&**array)].clone(), index: values[&std::ptr::from_ref(&**index)].clone(), element: node.ty.clone() }, node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::FieldAccess { object, field } => LirOperand::Local(emit(LirRvalue::FieldLoad { object: values[&std::ptr::from_ref(&**object)].clone(), object_ty: object.ty.clone(), field: field.clone(), result: node.ty.clone() }, node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::StaticField { class, field } => {
                let value = if callables.resolution.enums.contains_key(class) { LirRvalue::EnumAlloc { class: *class, variant: field.clone(), enum_ty: node.ty.clone() } }
                    else { LirRvalue::StaticField { class: *class, field: field.clone(), result: node.ty.clone() } };
                LirOperand::Local(emit(value, node.ty.clone(), node.span, emitted, locals, names))
            },
            HirExprKind::Range { start, end } => LirOperand::Local(emit(LirRvalue::Range { start: values[&std::ptr::from_ref(&**start)].clone(), end: values[&std::ptr::from_ref(&**end)].clone() }, node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::Int(value) => LirOperand::Int(*value),
            HirExprKind::Float(value) => LirOperand::Float(*value),
            HirExprKind::Bool(value) => LirOperand::Bool(*value),
            HirExprKind::Str(value) => LirOperand::Local(emit(LirRvalue::StringLiteral(value.clone()), node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::FnRef(function) => LirOperand::Local(emit(LirRvalue::FunctionRef { function: function.clone(), ty: node.ty.clone() }, node.ty.clone(), node.span, emitted, locals, names)),
            HirExprKind::Lambda { id, captures, .. } => {
                let value = LirRvalue::Closure { id: *id, captures: captures.iter().map(|capture| LirOperand::Local(names[&capture.source])).collect(), ty: node.ty.clone() };
                LirOperand::Local(emit(value, node.ty.clone(), node.span, emitted, locals, names))
            }
            HirExprKind::Print { value, newline } => {
                let value = LirRvalue::Print { value: values[&std::ptr::from_ref(&**value)].clone(), ty: value.ty.clone(), newline: *newline };
                LirOperand::Local(emit(value, node.ty.clone(), node.span, emitted, locals, names))
            }
            HirExprKind::Var(name) => {
                let source = LirOperand::Local(names[name]);
                if snapshots { LirOperand::Local(emit(LirRvalue::Use(source), node.ty.clone(), node.span, emitted, locals, names)) } else { source }
            }
            HirExprKind::MethodCall { object, method, args } => {
                let receiver = values[&std::ptr::from_ref(&**object)].clone();
                let operands = args.iter().map(|arg| values[&std::ptr::from_ref(arg)].clone()).collect();
                let arg_types = args.iter().map(|arg| values[&std::ptr::from_ref(arg)].ty(locals).expect("typed argument")).collect();
                let value = if let Some(resolved) = crate::semantic::intrinsics::resolve(&object.ty, method, args.len()) {
                    LirRvalue::IntrinsicCall { intrinsic: resolved.intrinsic, method: method.clone(), receiver, receiver_ty: object.ty.clone(), args: operands, arg_types, result: node.ty.clone() }
                } else if enum_method(&object.ty, method) { LirRvalue::EnumMethod { receiver, receiver_ty: object.ty.clone(), method: method.clone(), args: operands, arg_types, result: node.ty.clone() } }
                else { LirRvalue::MethodCall { receiver, receiver_ty: object.ty.clone(), method: method.clone(), args: operands, arg_types, result: node.ty.clone() } };
                LirOperand::Local(emit(value, node.ty.clone(), node.span, emitted, locals, names))
            }
            HirExprKind::Call { callee, args } => {
                let operands = args.iter().map(|arg| values[&std::ptr::from_ref(arg)].clone()).collect();
                let value = if let Some(callable) = values.get(&std::ptr::from_ref(node)) {
                    let ty = callable.ty(locals).expect("callable local");
                    let (Type::Fn(params, result) | Type::Closure(params, result)) = &ty else { unreachable!() };
                    LirRvalue::IndirectCall { callee: callable.clone(), name: *callee, args: operands, params: params.clone(), result: (**result).clone() }
                } else if let Some((params, result)) = callables.get(callee) {
                    if let Some(output) = callables.async_outputs.get(callee) { LirRvalue::StartTask { callee: *callee, args: operands, params: params.clone(), output: output.clone() } }
                    else { LirRvalue::DirectCall { callee: *callee, args: operands, params: params.clone(), result: result.clone() } }
                } else if callee.is_free_named("recover") { LirRvalue::Recover }
                else { LirRvalue::BuiltinCall { callee: *callee, args: operands, params: args.iter().map(|arg| arg.ty.clone()).collect(), result: node.ty.clone() } };
                LirOperand::Local(emit(value, node.ty.clone(), node.span, emitted, locals, names))
            }
            HirExprKind::Unary { op, operand } => {
                let value = LirRvalue::Unary {
                    op: op.clone(),
                    operand: values[&std::ptr::from_ref(&**operand)].clone(),
                    ty: node.ty.clone(),
                };
                LirOperand::Local(emit(
                    value,
                    node.ty.clone(),
                    node.span,
                    emitted,
                    locals,
                    names,
                ))
            }
            HirExprKind::Binary { op, lhs, rhs } => {
                let value = LirRvalue::Binary {
                    op: op.clone(),
                    lhs: values[&std::ptr::from_ref(&**lhs)].clone(),
                    rhs: values[&std::ptr::from_ref(&**rhs)].clone(),
                    operand_ty: lhs.ty.clone(),
                };
                LirOperand::Local(emit(
                    value,
                    node.ty.clone(),
                    node.span,
                    emitted,
                    locals,
                    names,
                ))
            }
            _ => unreachable!("scalar tree was validated"),
        };
        values.insert(std::ptr::from_ref(node), operand);
    }
    let LirOperand::Local(result) = values[&std::ptr::from_ref(&*expr)] else {
        unreachable!()
    };
    expr.kind = HirExprKind::Var(locals[result.0 as usize].name.clone());
}

fn append_format(node: &HirExpr, piece: LirOperand, values: &mut HashMap<*const HirExpr, LirOperand>, emitted: &mut Vec<SourceInst>, locals: &mut Vec<LirLocal>, names: &mut HashMap<String, LirLocalId>) {
    let key = std::ptr::from_ref(node);
    let result = if let Some(previous) = values.get(&key) {
        LirOperand::Local(emit(LirRvalue::Binary { op: BinOp::Add, lhs: previous.clone(), rhs: piece, operand_ty: Type::String }, Type::String, node.span, emitted, locals, names))
    } else { piece };
    values.insert(key, result);
}

fn reserve(
    ty: Type,
    locals: &mut Vec<LirLocal>,
    names: &mut HashMap<String, LirLocalId>,
) -> LirLocalId {
    let local = LirLocalId(locals.len() as u32);
    let mut name = format!("__lir_value_{}", local.0);
    while names.contains_key(&name) {
        name.push('_');
    }
    locals.push(LirLocal {
        storage_kind: super::super::LirStorageKind::Value,
        id: local,
        name: name.clone(),
        ty,
        source_span: None,
        synthetic: true,
        parameter: false,
    });
    names.insert(name, local);
    local
}

fn emit(value: LirRvalue, ty: Type, span: Span, emitted: &mut Vec<SourceInst>, locals: &mut Vec<LirLocal>, names: &mut HashMap<String, LirLocalId>) -> LirLocalId {
    let local = reserve(ty, locals, names);
    emitted.push(SourceInst::Compute { local, value, span });
    local
}

pub(in crate::ir::lowered) fn lower_blocks(blocks: &mut [SourceBlock], locals: &mut Vec<LirLocal>, is_async: bool) {
    lower_blocks_with(blocks, locals, &Callables::default(), is_async);
}

pub(in crate::ir::lowered) fn lower_calls(functions: &mut [super::super::SourceFunction], lambdas: &mut [super::super::SourceLambda], resolution: &crate::ir::typed_ast::HirResolution) {
    let mut callables = Callables { signatures: HashMap::new(), async_outputs: HashMap::new(), resolution: resolution.clone() };
    for (name, signature) in &resolution.functions {
        if name.is_free_named(name.unqualified_name()) && (matches!(name.unqualified_name(), "panic" | "recover" | "format") || crate::semantic::intrinsics::builtin_call_runtime_name(name.unqualified_name()).is_some()) { continue; }
        let result = if signature.is_async { callables.async_outputs.insert(*name, signature.return_type.clone()); Type::Generic(TypeId::local("Task"), vec![signature.return_type.clone()]) } else { signature.return_type.clone() };
        callables.signatures.insert(*name, (signature.params.clone(), result));
    }
    for f in functions.iter() {
        let result = if f.is_async { callables.async_outputs.insert(f.name, f.return_type.clone()); Type::Generic(TypeId::local("Task"), vec![f.return_type.clone()]) } else { f.return_type.clone() };
        callables.signatures.insert(f.name, (f.params.iter().map(|param| param.ty.clone()).collect(), result));
    }
    let mut pending: Vec<_> = functions.iter_mut().chain(lambdas.iter_mut().map(|lambda| &mut lambda.function)).collect();
    while let Some(f) = pending.pop() {
        lower_blocks_with(&mut f.blocks, &mut f.locals, &callables, f.is_async);
        if f.is_async { f.async_frame = super::super::async_liveness::analyze(&f.blocks, &f.locals); }
        for block in &mut f.blocks {
            for inst in &mut block.instrs {
                if let SourceInst::Defer { body, .. } = inst { pending.push(&mut body.function); }
            }
        }
    }
}

fn operand(expr: &mut HirExpr, emitted: &mut Vec<SourceInst>, locals: &mut Vec<LirLocal>, names: &mut HashMap<String, LirLocalId>, callables: &Callables, is_async: bool, snapshot: bool) -> LirOperand {
    lower(expr, emitted, locals, names, callables, is_async);
    let value = match &expr.kind {
        HirExprKind::Var(name) => LirOperand::Local(names[name]),
        HirExprKind::Int(value) => LirOperand::Int(*value),
        HirExprKind::Float(value) => LirOperand::Float(*value),
        HirExprKind::Bool(value) => LirOperand::Bool(*value),
        _ => unreachable!("validated expression became a flat operand"),
    };
    if snapshot && matches!(value, LirOperand::Local(_)) {
        LirOperand::Local(emit(LirRvalue::Use(value), expr.ty.clone(), expr.span, emitted, locals, names))
    } else { value }
}

fn lower_store(inst: &mut SourceInst, emitted: &mut Vec<SourceInst>, locals: &mut Vec<LirLocal>, names: &mut HashMap<String, LirLocalId>, callables: &Callables, is_async: bool) -> bool {
    let valid = match &*inst {
        SourceInst::FieldAssign { object, value, .. } => [object, value].iter().all(|expr| scalar(expr, names, locals, callables, is_async)),
        SourceInst::IndexAssign { array, index, value } => [array, index, value].iter().all(|expr| scalar(expr, names, locals, callables, is_async)),
        SourceInst::StaticFieldAssign { value, .. } => scalar(value, names, locals, callables, is_async),
        _ => false,
    };
    if !valid { return false; }
    let (value, span) = match inst {
        SourceInst::FieldAssign { object, field, value } => {
            let object_ty = object.ty.clone();
            let receiver = operand(object, emitted, locals, names, callables, is_async, true);
            let stored = operand(value, emitted, locals, names, callables, is_async, false);
            (LirRvalue::FieldStore { object: receiver, object_ty, field: field.clone(), value: stored }, object.span)
        }
        SourceInst::IndexAssign { array, index, value } => {
            let Type::Array(element) = &array.ty else { return false; };
            let element = (**element).clone();
            let receiver = operand(array, emitted, locals, names, callables, is_async, true);
            let index_value = operand(index, emitted, locals, names, callables, is_async, true);
            let stored = operand(value, emitted, locals, names, callables, is_async, false);
            (LirRvalue::ArrayStore { array: receiver, index: index_value, value: stored, element }, array.span)
        }
        SourceInst::StaticFieldAssign { class, field, value } => {
            let stored = operand(value, emitted, locals, names, callables, is_async, false);
            (LirRvalue::StaticStore { class: *class, field: field.clone(), value: stored }, value.span)
        }
        _ => unreachable!(),
    };
    emit(value, Type::Void, span, emitted, locals, names);
    true
}

fn lower_blocks_with(blocks: &mut [SourceBlock], locals: &mut Vec<LirLocal>, callables: &Callables, is_async: bool) {
    let mut names = locals
        .iter()
        .map(|local| (local.name.clone(), local.id))
        .collect();
    for block in blocks {
        let mut emitted = Vec::new();
        for mut inst in std::mem::take(&mut block.instrs) {
            if lower_store(&mut inst, &mut emitted, locals, &mut names, callables, is_async) { continue; }
            match &mut inst {
                SourceInst::Let { value, ty, .. } => {
                    if matches!(&value.kind, HirExprKind::StaticCall { class, method, args } if *class == TypeId::local("Map") && method == "new" && args.is_empty()) {
                        value.ty = ty.clone();
                    }
                    lower(value, &mut emitted, locals, &mut names, callables, is_async)
                }
                SourceInst::Assign { value, .. }
                | SourceInst::Expr(value) => lower(value, &mut emitted, locals, &mut names, callables, is_async),
                _ => {}
            }
            if !matches!(&inst, SourceInst::Expr(HirExpr { ty: Type::Never, kind: HirExprKind::Var(_), .. })) { emitted.push(inst); }
        }
        match &mut block.terminator {
            SourceTerminator::Branch { cond, .. } | SourceTerminator::Return(Some(cond)) => {
                lower(cond, &mut emitted, locals, &mut names, callables, is_async)
            }
            _ => {}
        }
        block.instrs = emitted;
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn program(source: &str) -> super::super::super::SourceProgram {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&ast);
        let tables = crate::ir::lower::CheckerTables::from_checker(&checker);
        let (hir, errors) = crate::ir::lower::lower_program_with(&ast, &tables);
        assert!(errors.is_empty(), "{errors:?}");
        super::super::super::lower_source_program(&hir)
    }

    #[test]
    fn array_reference_owner_is_captured_before_later_argument_cfg() {
        let program = program("fn write(value: &mut i64, ignored: i64) { value = 9; } fn f(c: bool) { let a = [1]; write(&a[0], c ? gc_allocated_bytes() : 0); }");
        let function = program.functions.iter().find(|function| function.name.is_free_named("f")).unwrap();
        let capture = function.blocks[0].instrs.iter().position(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::CaptureArrayOwner { .. }, .. })).expect("capture before branch");
        let debug = function.blocks[0].instrs.iter().position(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::ReferenceDebug { .. }, .. })).unwrap();
        assert!(debug < capture);
        assert!(matches!(function.blocks[0].terminator, SourceTerminator::Branch { .. }));
        assert!(function.blocks.iter().flat_map(|block| &block.instrs).any(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::DirectCall { args, .. }, .. } if matches!(args.first(), Some(LirOperand::Reference { place: LirPlace::ArrayElement { .. }, .. })))));
    }

    #[test]
    fn propagation_is_cfg_and_reuses_the_existing_error_object() {
        let program = program(r#"fn f(value: Result<i64, String>) -> Result<String, String> { let number = value?; return Result::Ok("ok"); }"#);
        let function = &program.functions[0];
        assert!(function.blocks.iter().any(|block| matches!(block.terminator, SourceTerminator::Branch { .. })));
        assert!(function.blocks.iter().flat_map(|block| &block.instrs).any(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::RebindResultError { .. }, .. })));
        for block in &function.blocks {
            for inst in &block.instrs {
                if let SourceInst::Let { value, .. } | SourceInst::Assign { value, .. } | SourceInst::Expr(value) = inst {
                    assert!(!value.walk_postorder(false).any(|node| matches!(node.kind, HirExprKind::TryPropagate { .. })));
                }
            }
        }
    }

    #[test]
    fn constructor_shell_precedes_conditional_argument_cfg() {
        let program = program("class Box { pub value: i64; } fn f(c: bool) -> Box { return new Box(c ? gc_allocated_bytes() : 0); }");
        let function = &program.functions[0];
        assert!(function.blocks[0].instrs.iter().any(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::ObjectAlloc { .. }, .. })));
        assert!(matches!(function.blocks[0].terminator, SourceTerminator::Branch { .. }));
        assert!(function.blocks.iter().flat_map(|block| &block.instrs).any(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::FieldStore { .. }, .. })));
    }

    #[test]
    fn enum_shell_precedes_payload_evaluation() {
        let program = program("enum Pair { Values(i64, i64) } fn f() -> Pair { return Pair::Values(gc_allocated_bytes(), gc_allocated_bytes()); }");
        let kinds: Vec<_> = program.functions[0].blocks.iter().flat_map(|block| &block.instrs).filter_map(|inst| match inst {
            SourceInst::Compute { value: LirRvalue::EnumAlloc { .. }, .. } => Some("alloc"),
            SourceInst::Compute { value: LirRvalue::EnumPayloadStore { .. }, .. } => Some("store"),
            SourceInst::Compute { value: LirRvalue::BuiltinCall { .. }, .. } => Some("call"),
            _ => None,
        }).collect();
        assert_eq!(kinds, ["alloc", "call", "store", "call", "store"]);
    }

    #[test]
    fn formatting_finishes_each_segment_before_next_operand() {
        let program = program(r#"fn f() -> String { return format("{}:{}", gc_allocated_bytes(), gc_allocated_bytes()); }"#);
        let kinds: Vec<_> = program.functions[0].blocks.iter().flat_map(|block| &block.instrs).filter_map(|inst| match inst {
            SourceInst::Compute { value: LirRvalue::BuiltinCall { .. }, .. } => Some("call"),
            SourceInst::Compute { value: LirRvalue::FormatScalar { .. }, .. } => Some("format"),
            SourceInst::Compute { value: LirRvalue::StringLiteral(_), .. } => Some("literal"),
            SourceInst::Compute { value: LirRvalue::Binary { operand_ty: Type::String, .. }, .. } => Some("concat"),
            _ => None,
        }).collect();
        assert_eq!(kinds, ["call", "format", "literal", "concat", "call", "format", "concat"]);
    }

    #[test]
    fn array_allocation_and_stores_interleave_element_evaluation() {
        let program = program("fn f() -> Array<i64> { return [gc_allocated_bytes(), gc_allocated_bytes()]; }");
        let values: Vec<_> = program.functions[0].blocks.iter().flat_map(|block| &block.instrs)
            .filter_map(|inst| if let SourceInst::Compute { value, .. } = inst { Some(value) } else { None }).collect();
        let kinds: Vec<_> = values.iter().filter_map(|value| match value {
            LirRvalue::ArrayAlloc { .. } => Some("alloc"),
            LirRvalue::ArrayStore { .. } => Some("store"),
            LirRvalue::BuiltinCall { .. } => Some("call"),
            _ => None,
        }).collect();
        assert_eq!(kinds, ["alloc", "call", "store", "call", "store"]);
        for inst in program.functions[0].blocks.iter().flat_map(|block| &block.instrs) {
            if let SourceInst::Compute { local, value, .. } = inst { assert!(value.is_well_typed(&program.functions[0].locals, *local), "{value:?}"); }
        }
    }
}
