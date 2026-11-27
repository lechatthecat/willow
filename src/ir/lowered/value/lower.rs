use super::LirPlace;
use super::*;
use crate::ir::lowered::{
    HirCallBinding, LoweringDeclarations, ParameterType, ProjectedParameter, ResolvedSignature,
    match_bound_parameter,
};
use crate::semantic::builtin_types::BuiltinTypeId as Builtin;
use std::borrow::Cow;

#[derive(Clone, Copy)]
enum CallParameters<'a> {
    Source(&'a [crate::ir::typed_ast::HirParam]),
    Declaration(&'a [Type]),
}

impl<'a> CallParameters<'a> {
    fn iter(self) -> impl ExactSizeIterator<Item = &'a Type> {
        let len = match self {
            Self::Source(params) => params.len(),
            Self::Declaration(types) => types.len(),
        };
        (0..len).map(move |i| match self {
            Self::Source(params) => &params[i].ty,
            Self::Declaration(types) => &types[i],
        })
    }
}

struct CallSignature<'a> {
    params: CallParameters<'a>,
    output: &'a Type,
    is_async: bool,
}

impl CallSignature<'_> {
    fn returns(&self, ty: &Type) -> bool {
        if self.is_async {
            matches!(ty, Type::Generic(id, args) if *id == TypeId::local("Task")
                && args.as_slice() == std::slice::from_ref(self.output))
        } else {
            self.output == ty
        }
    }
}

struct Callables<'s, 'r> {
    signatures: HashMap<FunctionId, CallSignature<'s>>,
    resolution: &'r LoweringDeclarations<'r>,
}
impl<'r> Callables<'_, 'r> {
    fn new(resolution: &'r LoweringDeclarations<'r>) -> Self {
        Self {
            resolution,
            signatures: HashMap::new(),
        }
    }
}
fn static_field(class: &TypeId, field: &str, callables: &Callables) -> bool {
    let mut next = Some(*class);
    let mut seen = std::collections::HashSet::new();
    while let Some(class) = next {
        if !seen.insert(class) {
            return false;
        }
        let Some(info) = callables.resolution.classes.get(&class) else {
            return false;
        };
        if info.static_fields.contains_key(field) {
            return true;
        }
        next = info.base;
    }
    false
}

fn static_call<'a>(
    class: &TypeId,
    method: &str,
    args: &[HirExpr],
    result: &Type,
    callables: &'a Callables,
    plan: &mut ScalarPlan<'a>,
) -> bool {
    if let Some(namespace) = callables.resolution.namespaces.get(class)
        && let Some(signature) = crate::semantic::intrinsics::namespace_builtin(namespace, method)
    {
        return signature.ret == *result
            && plan.parameters(Cow::Owned(signature.params), args, callables.resolution);
    }
    if let Some(signature) = super::super::static_signature(callables.resolution, class, method) {
        if !signature.declaration.is_static || signature.declaration.params.len() != args.len() {
            return false;
        }
        let output_matches = if signature.declaration.is_async {
            matches!(result, Type::Generic(id, args) if *id == TypeId::local("Task")
                && args.as_slice() == std::slice::from_ref(signature.return_type().as_ref()))
        } else {
            signature.return_type().as_ref() == result
        };
        return output_matches && plan.method_parameters(&signature, args, callables.resolution);
    }
    if callables.resolution.classes.contains_key(class)
        || callables.resolution.enums.contains_key(class)
    {
        return false;
    }
    let class_name = class.to_string();
    let builtin = Builtin::from_name(&class_name);
    let valid = match (builtin, method, args) {
        (
            Some(Builtin::Map | Builtin::Channel | Builtin::CancellationToken | Builtin::TaskScope),
            "new",
            [],
        ) => true,
        (Some(Builtin::Channel), "with_capacity", [arg]) => arg.ty == Type::I64,
        (id, "new", [arg])
            if matches!(
                id,
                Some(
                    Builtin::AtomicI64
                        | Builtin::AtomicBool
                        | Builtin::Mutex
                        | Builtin::RwLock
                        | Builtin::BlockingCell
                        | Builtin::BlockingRwCell
                )
            ) || matches!(class_name.as_str(), "CellI64" | "CellBool") =>
        {
            use crate::semantic::builtin_types;
            matches!(result, Type::Generic(name, payloads) if name == class && matches!(payloads.as_slice(), [ty] if argument_coercible(ty, arg, callables.resolution)))
                || builtin_types::resolve(result)
                    .filter(|resolved| {
                        !matches!(
                            resolved.id,
                            Builtin::Mutex | Builtin::RwLock | Builtin::Range
                        )
                    })
                    .is_some_and(|resolved| {
                        resolved
                            .args
                            .first()
                            .is_some_and(|ty| argument_coercible(ty, arg, callables.resolution))
                    })
                || matches!(
                    (id, &arg.ty),
                    (Some(Builtin::AtomicI64), Type::I64) | (Some(Builtin::AtomicBool), Type::Bool)
                )
                || matches!(
                    (class_name.as_str(), &arg.ty),
                    ("CellI64", Type::I64) | ("CellBool", Type::Bool)
                )
        }
        (None, "new", []) if matches!(class_name.as_str(), "CancelToken" | "CancelSource") => true,
        // Only these builtin constructor and argument shapes have flat lowering.
        _ => false,
    };
    if valid
        && method == "new"
        && matches!(
            builtin,
            Some(
                Builtin::Mutex | Builtin::RwLock | Builtin::BlockingCell | Builtin::BlockingRwCell
            )
        )
        && let Type::Generic(_, params) = result
    {
        for (arg, target) in args.iter().zip(params) {
            if !exits_before_conversion(arg, plan.builtin_panic)
                && arg.ty != *target
                && !matches!(arg.kind, HirExprKind::ReferenceArg { .. })
            {
                plan.coercions
                    .get_or_insert_with(HashMap::new)
                    .insert(std::ptr::from_ref(arg), Parameter::ResultArgument);
            }
        }
    }
    valid
}

fn enum_variant_supported(
    class: &TypeId,
    variant: &str,
    ty: &Type,
    args: &[HirExpr],
    callables: &Callables,
) -> bool {
    let Some((info, variant)) = callables.resolution.enum_variant(class, variant, None) else {
        return false;
    };
    let type_args = match ty {
        Type::Generic(name, args) if name == class => args.as_slice(),
        Type::Named(name) if name == class => &[],
        _ => return false,
    };
    if type_args.len() != info.type_params.as_slice().len() {
        return false;
    }
    if matches!(variant.payloads.as_slice(), [] | [Type::Void]) {
        return args.is_empty();
    }
    // Resolve names only when a payload mentions one. The index belongs to
    // the declaration; each construction borrows its own actual arguments.
    if let [Type::Named(id)] = variant.payloads.as_slice()
        && info
            .type_params
            .position(id)
            .is_some_and(|i| matches!(type_args[i], Type::Void))
    {
        return args.is_empty();
    }
    variant.payloads.len() == args.len()
        && variant.payloads.iter().zip(args).all(|(param, arg)| {
            let matches = |ty: &Type, relax| {
                match_bound_parameter(param, ty, relax, &info.type_params, type_args, |_, _| None)
            };
            if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                matches(&arg.ty, false)
            } else {
                matches!(arg.ty, Type::Never)
                    || callables.resolution.can_coerce_matching(&arg.ty, matches)
            }
        })
}

fn constructor_supported<'a>(
    class: &TypeId,
    args: &[HirExpr],
    callables: &'a Callables,
    plan: &mut ScalarPlan<'a>,
) -> bool {
    let Some(info) = callables.resolution.classes.get(class) else {
        return false;
    };
    if let Some(signature) = &info.constructor {
        return plan.parameters(Cow::Borrowed(&signature.params), args, callables.resolution);
    }
    callables
        .resolution
        .fields_rev(class)
        .is_some_and(|fields| fields.len() == args.len())
}

fn builtin_call(
    callee: &FunctionId,
    args: &[HirExpr],
    result: &Type,
    check_format: impl FnOnce() -> bool,
) -> bool {
    let name = callee.unqualified_name();
    let name = name.as_deref().unwrap_or("");
    if !callee.is_free_named(name) {
        return false;
    }
    if name == "$contract_i64" {
        return matches!(args, [op, lhs, rhs] if op.ty == Type::I64
            && lhs.ty.integer_layout().is_some() && lhs.ty == rhs.ty && lhs.ty == *result);
    }
    if name == "$contract_check" {
        return matches!(args, [a, b] if a.ty == Type::Bool && b.ty == Type::I64)
            && *result == Type::Void;
    }
    if name == "format" || (name == "panic" && args.len() > 1) {
        if (name == "format" && *result != Type::String)
            || (name == "panic" && *result != Type::Never)
        {
            return false;
        }
        return check_format();
    }
    if name == "panic" {
        return *result == Type::Never
            && (args.is_empty() || matches!(args, [arg] if arg.ty == Type::String));
    }
    if name == "recover" {
        return args.is_empty();
    }
    if name == "sleep" {
        return matches!(args, [arg] if arg.ty == Type::I64)
            && (*result == Type::Void
                || matches!(result, Type::Generic(name, payloads) if crate::semantic::builtin_types::resolve_name(name) == Some(Builtin::Future) && payloads == &[Type::Void]));
    }
    if name == "yield" {
        return args.is_empty()
            && (*result == Type::Void
                || matches!(result, Type::Generic(name, payloads) if crate::semantic::builtin_types::resolve_name(name) == Some(Builtin::Future) && payloads == &[Type::Void]));
    }
    crate::semantic::intrinsics::builtin_call_runtime_name(name).is_some()
        && args.is_empty()
        && if matches!(
            name,
            "gc_collect" | "gc_minor_collect" | "gc_task_frame_diagnostics"
        ) {
            *result == Type::Void
        } else {
            *result == Type::I64
        }
}

fn instance_method<'e>(
    receiver: &'e Type,
    super_class: Option<TypeId>,
    method: &str,
    args: &[HirExpr],
    callables: &'e Callables,
    plan: &mut ScalarPlan<'e>,
) -> bool {
    let resolution = callables.resolution;
    let signature = match super_class {
        Some(class) => super::super::lookup_method_signature(resolution, class, None, method),
        None => super::super::method_signature(resolution, receiver, method),
    };
    signature.is_some_and(|signature| {
        // Interface methods retain their receiver with either declaration spelling.
        (!signature.declaration.is_static
            || signature
                .interface
                .as_ref()
                .is_some_and(|i| resolution.interfaces.contains_key(&i.class)))
            && plan.method_parameters(&signature, args, resolution)
    })
}

/// The one type both sides of a class/interface `==` or `!=` are compared
/// at: the supertype side's, so the subtype side is coerced to
/// it first. Reference identity is then the object word on either side.
fn identity_operand_type<'e>(
    lhs: &'e HirExpr,
    rhs: &'e HirExpr,
    resolution: &crate::ir::typed_ast::HirResolution,
) -> Option<&'e Type> {
    let identity = |ty: &Type| {
        matches!(ty, Type::Named(id) | Type::Generic(id, _)
            if resolution.classes.contains_key(id) || resolution.interfaces.contains_key(id))
    };
    if !identity(&lhs.ty) || !identity(&rhs.ty) {
        return None;
    }
    if resolution.can_coerce(&rhs.ty, &lhs.ty) {
        Some(&lhs.ty)
    } else if resolution.can_coerce(&lhs.ty, &rhs.ty) {
        Some(&rhs.ty)
    } else {
        None
    }
}

fn argument_coercible(
    target: &Type,
    arg: &HirExpr,
    resolution: &crate::ir::typed_ast::HirResolution,
) -> bool {
    if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
        *target == arg.ty
    } else {
        arg.ty == Type::Never || resolution.can_coerce(&arg.ty, target)
    }
}

// Store preflight can check several operands before lowering any of them.
// Remember the side, so those operands can later be lowered in place.
#[derive(Clone, Copy)]
enum IdentitySide {
    Left,
    Right,
}

enum Parameter<'a> {
    Declared(Cow<'a, Type>),
    Projected(ProjectedParameter<'a>),
    // Generic cell and lock constructors take their parameter from the HIR result.
    ResultArgument,
}

#[derive(Default)]
struct ScalarPlan<'a> {
    snapshots: bool,
    // Postorder validation identifies the builtin after local and declared
    // callables have had their chance to resolve the name.
    builtin_panic: Option<FunctionId>,
    // A named function already carries its ID in HIR. Only local calls need a slot.
    local_callees: HashMap<FunctionId, LirLocalId>,
    identities: Option<HashMap<*const HirExpr, IdentitySide>>,
    coercions: Option<HashMap<*const HirExpr, Parameter<'a>>>,
    formats: Option<HashMap<*const HirExpr, crate::interpolate::CheckedTemplate<'a>>>,
}

// These are the scalar evaluator's two explicit exits. Other calls returning
// Never still use the ordinary call instruction and its existing continuation.
fn exits_before_conversion(arg: &HirExpr, builtin_panic: Option<FunctionId>) -> bool {
    matches!(arg.ty, Type::Never)
        && match &arg.kind {
            HirExprKind::Var(_) => true,
            HirExprKind::Call {
                callee,
                binding: HirCallBinding::Function,
                ..
            } => Some(*callee) == builtin_panic,
            _ => false,
        }
}

impl<'a> ScalarPlan<'a> {
    fn local_callee(
        &mut self,
        callee: FunctionId,
        names: &HashMap<String, LirLocalId>,
    ) -> Option<LirLocalId> {
        use std::collections::hash_map::Entry;
        match self.local_callees.entry(callee) {
            Entry::Occupied(slot) => Some(*slot.get()),
            Entry::Vacant(slot) => {
                let id = *names.get(callee.name().as_ref())?;
                Some(*slot.insert(id))
            }
        }
    }

    fn method_parameters(
        &mut self,
        signature: &ResolvedSignature<'a, 'a>,
        args: &[HirExpr],
        resolution: &crate::ir::typed_ast::HirResolution,
    ) -> bool {
        let builtin_panic = self.builtin_panic;
        signature.check_conversions(
            args,
            |arg| exits_before_conversion(arg, builtin_panic),
            |_, target, arg| {
                let parameter = match target {
                    ParameterType::Declared(ty) => {
                        if !argument_coercible(ty, arg, resolution) {
                            return false;
                        }
                        Parameter::Declared(Cow::Borrowed(ty))
                    }
                    ParameterType::Projected(p) => {
                        if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                            return false;
                        }
                        if !matches!(arg.ty, Type::Never) && !p.accepts_value(&arg.ty, resolution) {
                            return false;
                        }
                        Parameter::Projected(p)
                    }
                };
                self.coercions
                    .get_or_insert_with(HashMap::new)
                    .insert(std::ptr::from_ref(arg), parameter);
                true
            },
        )
    }

    fn parameters(
        &mut self,
        params: Cow<'a, [Type]>,
        args: &[HirExpr],
        resolution: &crate::ir::typed_ast::HirResolution,
    ) -> bool {
        match params {
            Cow::Borrowed(params) => {
                self.check_parameters(params.iter().map(Cow::Borrowed), args, resolution)
            }
            Cow::Owned(params) => {
                self.check_parameters(params.into_iter().map(Cow::Owned), args, resolution)
            }
        }
    }

    fn check_parameters(
        &mut self,
        params: impl ExactSizeIterator<Item = Cow<'a, Type>>,
        args: &[HirExpr],
        resolution: &crate::ir::typed_ast::HirResolution,
    ) -> bool {
        params.len() == args.len()
            && params.zip(args).all(|(target, arg)| {
                if exits_before_conversion(arg, self.builtin_panic) || *target == arg.ty {
                    return true;
                }
                if !argument_coercible(&target, arg, resolution) {
                    return false;
                }
                self.coercions
                    .get_or_insert_with(HashMap::new)
                    .insert(std::ptr::from_ref(arg), Parameter::Declared(target));
                true
            })
    }
}

fn scalar<'e>(
    expr: &'e HirExpr,
    names: &HashMap<String, LirLocalId>,
    locals: &[LirLocal],
    callables: &'e Callables,
    is_async: bool,
    plan: &mut ScalarPlan<'e>,
) -> bool {
    plan.snapshots = false;
    expr.walk_postorder(false).all(|node| match &node.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) | HirExprKind::Str(_) | HirExprKind::FnRef(_) => true,
        HirExprKind::ReferenceArg { place } => matches!(place.kind, HirExprKind::Var(_) | HirExprKind::FieldAccess { .. } | HirExprKind::Index { .. }),
        HirExprKind::New { class, args } => {
            plan.snapshots = true;
            constructor_supported(class, args, callables, plan)
        }
        HirExprKind::ObjectLiteral { class, fields } => super::super::object_literal_supported(callables.resolution, class, fields),
        HirExprKind::StaticField { class, field } => enum_variant_supported(class, field, &node.ty, &[], callables)
            || static_field(class, field, callables),
        HirExprKind::StaticCall { class, method, args } => {
            plan.snapshots = true;
            enum_variant_supported(class, method, &node.ty, args, callables)
                || static_call(class, method, args, &node.ty, callables, plan)
        }
        HirExprKind::Await { inner } => crate::semantic::builtin_types::unary_arg(&inner.ty, crate::semantic::builtin_types::BuiltinTypeId::Future) == Some(&node.ty)
            || (!is_async && node.ty == Type::Void && matches!(&inner.kind, HirExprKind::Call { callee, args, binding: HirCallBinding::Function } if (callee.is_free_named("sleep") || callee.is_free_named("yield")) && builtin_call(callee, args, &inner.ty, || false))),
        HirExprKind::Array { .. } => matches!(node.ty, Type::Array(_)),
        HirExprKind::Index { .. } | HirExprKind::FieldAccess { .. } | HirExprKind::Range { .. } => true,
        HirExprKind::Unary { .. } => matches!(node.ty, Type::I8 | Type::I16 | Type::I32 | Type::U16 | Type::U32 | Type::U64 | Type::Char | Type::U8 | Type::I64 | Type::F64 | Type::Bool),
        HirExprKind::Var(name) => names.contains_key(name),
        HirExprKind::Lambda { captures, .. } => captures.iter().all(|capture| names.contains_key(&capture.source)),
        HirExprKind::Print { value, .. } => matches!(value.ty, Type::I8 | Type::I16 | Type::I32 | Type::U16 | Type::U32 | Type::U64 | Type::Char | Type::U8 | Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Never),
        HirExprKind::MethodCall { object, method, args, super_class, builtin } => {
            plan.snapshots = true;
            match builtin {
                Some(id) => !(is_async && id.is_suspension_point()),
                None => instance_method(&object.ty, *super_class, method, args, callables, plan),
            }
        }
        HirExprKind::Invoke { callee, args, .. } => {
            plan.snapshots = true;
            matches!(&callee.ty, Type::Fn(params, result) | Type::Closure(params, result)
                if **result == node.ty && params.len() == args.len()
                    && !args.iter().any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                    && params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, callables.resolution)))
        }
        HirExprKind::Call { callee, binding, args } => {
            plan.snapshots = true;
            if binding.is_local() {
                let Some(local) = plan.local_callee(*callee, names) else { return false };
                matches!(&locals[local.0 as usize].ty, Type::Fn(params, result) | Type::Closure(params, result)
                    if !args.iter().any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                        && **result == node.ty && params.len() == args.len()
                        && params.iter().zip(args).all(|(ty, arg)| argument_coercible(ty, arg, callables.resolution)))
            } else if let Some(sig) = callables.signatures.get(callee) {
                sig.returns(&node.ty) && plan.check_parameters(sig.params.iter().map(Cow::Borrowed), args, callables.resolution)
            } else {
                let valid = builtin_call(callee, args, &node.ty, || {
                    super::super::check_format(args, |_| {}).is_some_and(|template| {
                        plan.formats.get_or_insert_with(HashMap::new).insert(std::ptr::from_ref(node), template);
                        true
                    })
                });
                if valid && matches!(node.ty, Type::Never) && callee.is_free_named("panic") {
                    plan.builtin_panic = Some(*callee);
                }
                valid
            }
        }
        HirExprKind::Binary { op, lhs, rhs } => !matches!(op, BinOp::And | BinOp::Or)
            && (matches!(lhs.ty, Type::I8 | Type::I16 | Type::I32 | Type::U16 | Type::U32 | Type::U64 | Type::Char | Type::U8 | Type::I64 | Type::F64 | Type::Bool | Type::String)
                || (matches!(op, BinOp::Eq | BinOp::Ne) && identity_operand_type(lhs, rhs, callables.resolution).inspect(|&target| {
                    if lhs.ty != rhs.ty {
                        let side = if std::ptr::eq(target, &lhs.ty) { IdentitySide::Left } else { IdentitySide::Right };
                        plan.identities.get_or_insert_with(HashMap::new).insert(std::ptr::from_ref(node), side);
                    }
                }).is_some())
                || (matches!(op, BinOp::Eq | BinOp::Ne) && matches!(&lhs.ty, Type::Named(name) | Type::Generic(name, _) if callables.resolution.enum_is_payload_free(name)))),
        HirExprKind::Ternary { .. } | HirExprKind::TryPropagate { .. } | HirExprKind::Match { .. } | HirExprKind::Select { .. } => false,
    })
}

mod scalar;
use scalar::lower;

fn append_format(
    node: &HirExpr,
    piece: LirOperand,
    values: &mut HashMap<*const HirExpr, LirOperand>,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &HashMap<String, LirLocalId>,
) {
    let key = std::ptr::from_ref(node);
    let result = if let Some(previous) = values.get(&key) {
        LirOperand::Local(emit(
            LirRvalue::Binary {
                op: BinOp::Add,
                lhs: previous.clone(),
                rhs: piece,
                operand_ty: Type::String,
            },
            Type::String,
            node.span,
            emitted,
            locals,
            names,
        ))
    } else {
        piece
    };
    values.insert(key, result);
}

#[cfg(test)]
std::thread_local! {
    static EMITTED_COMPUTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EMITTED_LITERAL_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RESERVED_LOCALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn push_compute(emitted: &mut Vec<SourceInst>, local: LirLocalId, value: LirRvalue, span: Span) {
    #[cfg(test)]
    {
        EMITTED_COMPUTES.set(EMITTED_COMPUTES.get() + 1);
        if let LirRvalue::StringLiteral(text) = &value {
            EMITTED_LITERAL_BYTES.set(EMITTED_LITERAL_BYTES.get() + text.len());
        }
    }
    emitted.push(SourceInst::Compute { local, value, span });
}

fn reserve(
    ty: Type,
    locals: &mut Vec<LirLocal>,
    names: &HashMap<String, LirLocalId>,
) -> LirLocalId {
    #[cfg(test)]
    RESERVED_LOCALS.set(RESERVED_LOCALS.get() + 1);
    let local = LirLocalId(locals.len() as u32);
    // Scalar results reference each other by ID. Their numeric names are unique
    // within this pass and only need to avoid names present at entry.
    let mut name = format!("__lir_value_{}", local.0);
    while names.contains_key(&name) {
        name.push('_');
    }
    locals.push(LirLocal {
        storage_kind: super::super::LirStorageKind::Value,
        id: local,
        name,
        ty,
        source_span: None,
        synthetic: true,
        parameter: false,
    });
    local
}

fn emit(
    value: LirRvalue,
    ty: Type,
    span: Span,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &HashMap<String, LirLocalId>,
) -> LirLocalId {
    let local = reserve(ty, locals, names);
    push_compute(emitted, local, value, span);
    local
}

pub(in crate::ir::lowered) fn lower_blocks(
    blocks: &mut [SourceBlock],
    locals: &mut Vec<LirLocal>,
    is_async: bool,
) {
    let hir = Default::default();
    let resolution = LoweringDeclarations::new(&hir);
    lower_blocks_with(blocks, locals, &Callables::new(&resolution), is_async);
}

pub(in crate::ir::lowered) fn lower_calls<'r>(
    functions: &mut [super::super::SourceFunction],
    lambdas: &mut [super::super::SourceLambda],
    resolution: &'r LoweringDeclarations<'r>,
) {
    let mut callables = Callables::new(resolution);
    // Bodies and local tables can be lowered while the immutable source headers
    // lend their types to calls (including calls made from lifted/deferred bodies).
    let mut pending = Vec::with_capacity(functions.len() + lambdas.len());
    for f in functions {
        callables.signatures.insert(
            f.name,
            CallSignature {
                params: CallParameters::Source(&f.params),
                output: &f.return_type,
                is_async: f.is_async,
            },
        );
        pending.push((&mut f.blocks, &mut f.locals, f.is_async));
    }
    for lambda in lambdas {
        let f = &mut lambda.function;
        pending.push((&mut f.blocks, &mut f.locals, f.is_async));
    }
    for (name, sig) in &resolution.functions {
        let std::collections::hash_map::Entry::Vacant(slot) = callables.signatures.entry(*name)
        else {
            continue;
        };
        let spelling = name.unqualified_name();
        let spelling = spelling.as_deref().unwrap_or("");
        if name.is_free_named(spelling)
            && (matches!(spelling, "panic" | "recover" | "format")
                || crate::semantic::intrinsics::builtin_call_runtime_name(spelling).is_some())
        {
            continue;
        }
        slot.insert(CallSignature {
            params: CallParameters::Declaration(&sig.params),
            output: &sig.return_type,
            is_async: sig.is_async,
        });
    }
    let mut dispatch = super::super::dispatch::DispatchEffects::default();
    while let Some((blocks, locals, is_async)) = pending.pop() {
        lower_blocks_with(blocks, locals, &callables, is_async);
        for block in blocks {
            for inst in &mut block.instrs {
                if let SourceInst::Compute {
                    value:
                        LirRvalue::MethodCall {
                            receiver_ty,
                            method,
                            is_async,
                            super_class,
                            result,
                            ..
                        },
                    ..
                } = inst
                {
                    let receiver = super_class.map(Type::Named);
                    let receiver = receiver.as_ref().unwrap_or(receiver_ty);
                    *is_async = super::super::method_is_async(resolution, receiver, method)
                        || (super_class.is_none()
                            && matches!(result, Type::Generic(name, _) if crate::semantic::builtin_types::resolve_name(name) == Some(crate::semantic::builtin_types::BuiltinTypeId::Task))
                            && matches!(receiver, Type::Named(class) | Type::Generic(class, _)
                            if dispatch.may_start_task(resolution, *class, method)));
                }
                if let SourceInst::Compute {
                    value:
                        LirRvalue::StaticCall {
                            class,
                            method,
                            is_async,
                            ..
                        },
                    ..
                } = inst
                {
                    *is_async = resolution
                        .modules
                        .get(class)
                        .and_then(|module| module.get(method))
                        .map_or_else(
                            || {
                                super::super::method_is_async(
                                    resolution,
                                    &Type::Named(*class),
                                    method,
                                )
                            },
                            |signature| signature.is_async,
                        );
                }
                if let SourceInst::Defer { body, .. } = inst {
                    let f = &mut body.function;
                    pending.push((&mut f.blocks, &mut f.locals, f.is_async));
                }
            }
        }
    }
}

fn lower_store(
    inst: &SourceInst,
    emitted: &mut Vec<SourceInst>,
    locals: &mut Vec<LirLocal>,
    names: &HashMap<String, LirLocalId>,
    callables: &Callables,
    is_async: bool,
) -> bool {
    let mut plan = ScalarPlan::default();
    // Each store operand carries its own snapshot policy into emission.
    let mut snapshots = [false; 3];
    let mut flags = snapshots.iter_mut();
    let mut valid_operand = |expr| {
        let valid = scalar(expr, names, locals, callables, is_async, &mut plan);
        *flags.next().expect("a store has at most three operands") = plan.snapshots;
        valid
    };
    let valid = match inst {
        SourceInst::FieldAssign { object, value, .. } => {
            [object, value].into_iter().all(&mut valid_operand)
        }
        SourceInst::IndexAssign {
            array,
            index,
            value,
        } => [array, index, value].into_iter().all(&mut valid_operand),
        SourceInst::StaticFieldAssign { value, .. } => valid_operand(value),
        ir_variants!(
            SourceInst;
            Compute, EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation, ClearScopeRoots, Defer, Let,
            Assign, SuperInit, SelectInit, SelectProbe, SelectPick, SelectUnregister,
            SelectCommit, ReleaseLock, MatchTest, MatchBind, Expr
        ) => false,
    };
    if !valid {
        return false;
    }
    let mut flags = snapshots.into_iter();
    let mut operand = |expr, snapshot: bool| -> Result<LirOperand, LirLocalId> {
        plan.snapshots = flags.next().expect("each store operand was validated");
        let value = if scalar::needs_lowering(expr) {
            LirOperand::Local(scalar::lower_validated(
                expr, emitted, locals, names, callables, is_async, &mut plan,
            )?)
        } else {
            LirOperand::from_flat_expr(expr, |name| Some(names[name]))
                .expect("validated store operand is flat")
        };
        if expr.ty == Type::Never
            && let LirOperand::Local(id) = value
        {
            return Err(id);
        }
        Ok(if snapshot && matches!(value, LirOperand::Local(_)) {
            LirOperand::Local(emit(
                LirRvalue::Use(value),
                expr.ty.clone(),
                expr.span,
                emitted,
                locals,
                names,
            ))
        } else {
            value
        })
    };
    let result = (|| -> Result<_, LirLocalId> {
        Ok(Some(match inst {
            SourceInst::FieldAssign {
                object,
                field,
                value,
            } => {
                let receiver = operand(object, true)?;
                let stored = operand(value, false)?;
                (
                    LirRvalue::FieldStore {
                        object: receiver,
                        object_ty: object.ty.clone(),
                        field: field.clone(),
                        value: stored,
                    },
                    object.span,
                )
            }
            SourceInst::IndexAssign {
                array,
                index,
                value,
            } => {
                let Type::Array(element) = &array.ty else {
                    return Ok(None);
                };
                let receiver = operand(array, true)?;
                let index_value = operand(index, true)?;
                let stored = operand(value, false)?;
                (
                    LirRvalue::ArrayStore {
                        array: receiver,
                        index: index_value,
                        value: stored,
                        element: (**element).clone(),
                    },
                    array.span,
                )
            }
            SourceInst::StaticFieldAssign {
                class,
                field,
                value,
            } => {
                let stored = operand(value, false)?;
                (
                    LirRvalue::StaticStore {
                        class: *class,
                        field: field.clone(),
                        value: stored,
                    },
                    value.span,
                )
            }
            ir_variants!(
                SourceInst;
                Compute, EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation, ClearScopeRoots, Defer, Let,
                Assign, SuperInit, SelectInit, SelectProbe, SelectPick, SelectUnregister,
                SelectCommit, ReleaseLock, MatchTest, MatchBind, Expr
            ) => unreachable!(),
        }))
    })();
    match result {
        Ok(Some((value, span))) => {
            emit(value, Type::Void, span, emitted, locals, names);
            true
        }
        Err(_) => true,
        Ok(None) => false,
    }
}

fn lower_blocks_with(
    blocks: &mut [SourceBlock],
    locals: &mut Vec<LirLocal>,
    callables: &Callables,
    is_async: bool,
) {
    let names = locals
        .iter()
        .map(|local| (local.name.clone(), local.id))
        .collect();
    for block in blocks {
        let mut emitted = Vec::new();
        for mut inst in std::mem::take(&mut block.instrs) {
            if lower_store(&inst, &mut emitted, locals, &names, callables, is_async) {
                continue;
            }
            match &mut inst {
                SourceInst::Let { value, ty, .. } => {
                    if matches!(&value.kind, HirExprKind::StaticCall { class, method, args } if crate::semantic::builtin_types::resolve_name(class) == Some(crate::semantic::builtin_types::BuiltinTypeId::Map) && method == "new" && args.is_empty())
                    {
                        value.ty = ty.clone();
                    }
                    lower(value, &mut emitted, locals, &names, callables, is_async)
                }
                SourceInst::Assign { value, .. } | SourceInst::Expr(value) => {
                    lower(value, &mut emitted, locals, &names, callables, is_async)
                }
                ir_variants!(
                    SourceInst;
                    Compute, EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation, ClearScopeRoots,
                    Defer, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit, SelectInit,
                    SelectProbe, SelectPick, SelectUnregister, SelectCommit, ReleaseLock,
                    MatchTest, MatchBind
                ) => {}
            }
            // Keep lexical scope markers even after a raise: backend cleanup
            // construction consumes them on exceptional paths as well.
            if !matches!(
                &inst,
                SourceInst::Let {
                    value: HirExpr {
                        ty: Type::Never,
                        kind: HirExprKind::Var(_),
                        ..
                    },
                    ..
                } | SourceInst::Assign {
                    value: HirExpr {
                        ty: Type::Never,
                        kind: HirExprKind::Var(_),
                        ..
                    },
                    ..
                } | SourceInst::Expr(HirExpr {
                    ty: Type::Never,
                    kind: HirExprKind::Var(_),
                    ..
                })
            ) {
                emitted.push(inst);
            }
        }
        match &mut block.terminator {
            SourceTerminator::Branch { cond, .. } | SourceTerminator::Return(Some(cond)) => {
                lower(cond, &mut emitted, locals, &names, callables, is_async);
                if cond.ty == Type::Never {
                    block.terminator = SourceTerminator::Return(None);
                }
            }
            SourceTerminator::Jump(_)
            | SourceTerminator::Suspend { .. }
            | SourceTerminator::Return(_)
            | SourceTerminator::CleanupReturn => {}
        }
        block.instrs = emitted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_signature_setup_borrows_overlapping_and_declaration_only_types() {
        use crate::ir::lowered::SourceFunction;
        use crate::ir::typed_ast::{HirParam, HirResolution, HirSignature};
        use crate::parser::ast::REBUILT_TYPE_NODES;

        for count in [1, 8, 32] {
            for depth in [0, 8, 32] {
                let ty = (0..depth).fold(Type::I64, |ty, _| Type::Array(Box::new(ty)));
                for source_headers in [false, true] {
                    for is_async in [false, true] {
                        let mut hir = HirResolution::default();
                        let mut functions = Vec::new();
                        for i in 0..count {
                            let name = FunctionId::free(format!("f{i}"));
                            hir.functions.insert(
                                name,
                                HirSignature {
                                    params: vec![ty.clone()],
                                    param_modes: vec![],
                                    return_type: ty.clone(),
                                    is_async,
                                    is_static: false,
                                },
                            );
                            if source_headers {
                                functions.push(SourceFunction {
                                    name,
                                    is_async,
                                    params: vec![HirParam {
                                        name: "x".into(),
                                        ty: ty.clone(),
                                        by_reference: false,
                                        span: Span::dummy(),
                                    }],
                                    return_type: ty.clone(),
                                    locals: vec![],
                                    blocks: vec![],
                                    async_frame: Default::default(),
                                    captures: vec![],
                                });
                            }
                        }
                        let view = LoweringDeclarations::new(&hir);
                        REBUILT_TYPE_NODES.set(0);
                        lower_calls(&mut functions, &mut [], &view);
                        let rebuilt = REBUILT_TYPE_NODES.get();
                        assert_eq!(rebuilt, 0);
                        println!(
                            "functions={count} depth={depth} overlap={source_headers} async={is_async} setup_rebuilt={rebuilt}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn scalar_signatures_emit_winning_parameters_and_async_outputs() {
        use crate::ir::lowered::{lower_function, tests::checked_hir};
        let mut hir = checked_hir(
            "
            fn local_sync(x: i64) -> i64 {return x;}
            async fn local_async(x: i64) -> i64 {return x;}
            fn imported_sync(x: i64) -> i64 {return x;}
            async fn imported_async(x: i64) -> i64 {return x;}
            fn probe() { local_sync(1); local_async(2); imported_sync(3); imported_async(4); }
        ",
        );
        // Give the shadowed declarations different types and async flags.
        for name in ["local_sync", "local_async"] {
            let sig = hir
                .resolution
                .functions
                .get_mut(&FunctionId::free(name))
                .unwrap();
            sig.params = vec![Type::Bool];
            sig.return_type = Type::Bool;
            sig.is_async = !sig.is_async;
        }
        let view = LoweringDeclarations::new(&hir.resolution);
        let mut functions: Vec<_> = hir
            .functions
            .iter()
            .filter(|f| {
                !f.name.is_free_named("imported_sync") && !f.name.is_free_named("imported_async")
            })
            .map(|f| lower_function(f, None, &view))
            .collect();
        lower_calls(&mut functions, &mut [], &view);
        let probe = functions
            .iter()
            .find(|f| f.name.is_free_named("probe"))
            .unwrap();
        let calls: Vec<_> = probe
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter_map(|inst| {
                let SourceInst::Compute { value, .. } = inst else {
                    return None;
                };
                let (id, params, ty, is_async) = match value {
                    LirRvalue::DirectCall {
                        callee,
                        params,
                        result,
                        ..
                    } => (callee, params, result, false),
                    LirRvalue::StartTask {
                        callee,
                        params,
                        output,
                        ..
                    } => (callee, params, output, true),
                    _ => return None,
                };
                assert_eq!(params.as_slice(), &[Type::I64]);
                assert_eq!(*ty, Type::I64);
                Some((id.to_string(), is_async))
            })
            .collect();
        assert_eq!(
            calls,
            [
                ("local_sync", false),
                ("local_async", true),
                ("imported_sync", false),
                ("imported_async", true)
            ]
            .map(|(name, a)| (name.to_owned(), a))
        );
    }

    fn builtin_call(callee: &FunctionId, args: &[HirExpr], result: &Type) -> bool {
        super::builtin_call(callee, args, result, || {
            crate::ir::lowered::check_format(args, |_| {}).is_some()
        })
    }

    fn check_scheduler_builtin_results(callee: &str, args: &[HirExpr]) {
        for (result, expected) in [
            (Type::Void, true),
            (Builtin::Future.apply(vec![Type::Void]), true),
            (Type::Named(TypeId::local("Future")), false),
            (Builtin::Future.apply(vec![]), false),
            (Builtin::Future.apply(vec![Type::Void, Type::Void]), false),
            (Builtin::Future.apply(vec![Type::I64]), false),
            (
                Type::Generic(TypeId::from("user::Future"), vec![Type::Void]),
                false,
            ),
            (
                Type::Generic(TypeId::local("FutureLike"), vec![Type::Void]),
                false,
            ),
            (Builtin::Task.apply(vec![Type::Void]), false),
            (Type::Array(Box::new(Type::Void)), false),
        ] {
            assert_eq!(
                builtin_call(&FunctionId::free_from_source_name(callee), args, &result),
                expected,
                "{callee}: {result:?}"
            );
        }
        assert!(!builtin_call(
            &FunctionId::free_from_source_name(&format!("user::{callee}")),
            args,
            &Type::Void
        ));
    }

    #[test]
    fn scheduler_builtin_sleep_requires_exact_future_void_and_duration() {
        let duration = HirExpr {
            kind: HirExprKind::Var("duration".into()),
            ty: Type::I64,
            span: Span::dummy(),
        };
        check_scheduler_builtin_results("sleep", std::slice::from_ref(&duration));
        let callee = FunctionId::free_from_source_name("sleep");
        for result in [Type::Void, Builtin::Future.apply(vec![Type::Void])] {
            assert!(!builtin_call(&callee, &[], &result));
            assert!(!builtin_call(
                &callee,
                &[duration.clone(), duration.clone()],
                &result
            ));
            let mut wrong = duration.clone();
            wrong.ty = Type::Bool;
            assert!(!builtin_call(&callee, &[wrong], &result));
        }
    }

    #[test]
    fn scheduler_builtin_yield_requires_exact_future_void_and_no_arguments() {
        check_scheduler_builtin_results("yield", &[]);
        let argument = HirExpr {
            kind: HirExprKind::Var("argument".into()),
            ty: Type::I64,
            span: Span::dummy(),
        };
        for result in [Type::Void, Builtin::Future.apply(vec![Type::Void])] {
            assert!(!builtin_call(
                &FunctionId::free_from_source_name("yield"),
                std::slice::from_ref(&argument),
                &result
            ));
        }
    }

    fn program(source: &str) -> super::super::super::SourceProgram {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&ast);
        let tables = crate::ir::lower::SemanticFacts::from_checker(&checker);
        let (hir, errors) = crate::ir::lower::lower_program_with(&ast, &tables);
        assert!(errors.is_empty(), "{errors:?}");
        super::super::super::lower_source_program(&hir)
    }

    #[test]
    fn never_lowering_counts_scale_with_branch_count() {
        for count in [8, 32, 128] {
            let mut source = String::from("fn f(flag: bool) { ");
            for index in 0..count {
                source.push_str(&format!(
                    "let x{index}: f64 = flag ? panic(\"stop\") : 2.5;"
                ));
            }
            source.push('}');
            let program = program(&source);
            let function = program
                .functions
                .iter()
                .find(|f| f.name.is_free_named("f"))
                .unwrap();
            let instructions: Vec<_> = function.blocks.iter().flat_map(|b| &b.instrs).collect();
            let raises = instructions
                .iter()
                .filter(|inst| {
                    matches!(
                        inst,
                        SourceInst::Compute {
                            value: LirRvalue::Panic { .. },
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(raises, count);
            assert!(!instructions.iter().any(|inst| matches!(
                inst,
                SourceInst::Assign {
                    value: HirExpr {
                        ty: Type::Never,
                        kind: HirExprKind::Var(_),
                        ..
                    },
                    ..
                }
            )));
            let metrics = (
                function.blocks.len(),
                instructions.len(),
                function.locals.len(),
            );
            println!(
                "never scaling: branches={count}, blocks={}, instructions={}, locals={}",
                metrics.0, metrics.1, metrics.2
            );
            assert_eq!(metrics, (3 * count + 1, 5 * count + 1, 5 * count + 1));
        }
    }

    #[test]
    fn array_reference_owner_is_captured_before_later_argument_cfg() {
        let program = program(
            "fn write(value: &mut i64, ignored: i64) { value = 9; } fn f(c: bool) { let a = [1]; write(&a[0], c ? gc_allocated_bytes() : 0); }",
        );
        let function = program
            .functions
            .iter()
            .find(|function| function.name.is_free_named("f"))
            .unwrap();
        let capture = function.blocks[0]
            .instrs
            .iter()
            .position(|inst| {
                matches!(
                    inst,
                    SourceInst::Compute {
                        value: LirRvalue::CaptureArrayOwner { .. },
                        ..
                    }
                )
            })
            .expect("capture before branch");
        let debug = function.blocks[0]
            .instrs
            .iter()
            .position(|inst| {
                matches!(
                    inst,
                    SourceInst::Compute {
                        value: LirRvalue::ReferenceDebug { .. },
                        ..
                    }
                )
            })
            .unwrap();
        assert!(debug < capture);
        assert!(matches!(
            function.blocks[0].terminator,
            SourceTerminator::Branch { .. }
        ));
        assert!(function.blocks.iter().flat_map(|block| &block.instrs).any(|inst| matches!(inst, SourceInst::Compute { value: LirRvalue::DirectCall { args, .. }, .. } if matches!(args.first(), Some(LirOperand::Reference { place: LirPlace::ArrayElement { .. }, .. })))));
    }

    #[test]
    fn propagation_is_cfg_and_reuses_the_existing_error_object() {
        let program = program(
            r#"fn f(value: Result<i64, String>) -> Result<String, String> { let number = value?; return Result::Ok("ok"); }"#,
        );
        let function = &program.functions[0];
        assert!(
            function
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. }))
        );
        assert!(
            function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    SourceInst::Compute {
                        value: LirRvalue::RebindResultError { .. },
                        ..
                    }
                ))
        );
        for block in &function.blocks {
            for inst in &block.instrs {
                if let SourceInst::Let { value, .. }
                | SourceInst::Assign { value, .. }
                | SourceInst::Expr(value) = inst
                {
                    assert!(
                        !value
                            .walk_postorder(false)
                            .any(|node| matches!(node.kind, HirExprKind::TryPropagate { .. }))
                    );
                }
            }
        }
    }

    #[test]
    fn object_literal_fields_match_once_in_any_order() {
        use crate::ir::lowered::object_literal_supported;
        use crate::ir::typed_ast::{HirClassInfo, HirResolution};

        let class = TypeId::local("Pair");
        let mut resolution = HirResolution::default();
        resolution.classes.insert(
            class,
            HirClassInfo {
                fields: vec![("left".into(), Type::I64), ("right".into(), Type::I64)],
                ..HirClassInfo::default()
            },
        );
        let resolution = LoweringDeclarations::new(&resolution);
        let field = |name: &str| {
            (
                name.to_owned(),
                HirExpr {
                    kind: HirExprKind::Int(1),
                    ty: Type::I64,
                    span: Span::dummy(),
                },
            )
        };
        assert!(object_literal_supported(
            &resolution,
            &class,
            &[field("right"), field("left")]
        ));
        for fields in [
            vec![field("left")],
            vec![field("left"), field("left")],
            vec![field("right"), field("extra")],
        ] {
            assert!(!object_literal_supported(&resolution, &class, &fields));
        }
    }

    #[test]
    fn inherited_field_validation_borrows_names_and_types() {
        use crate::ir::lowered::object_literal_supported;
        use crate::ir::typed_ast::HirClassInfo;
        use crate::parser::ast::REBUILT_TYPE_NODES;

        let base = TypeId::local("Base");
        let child = TypeId::local("Child");
        let field = |name: &str| {
            (
                name.to_owned(),
                HirExpr {
                    kind: HirExprKind::Int(1),
                    ty: Type::I64,
                    span: Span::dummy(),
                },
            )
        };
        let fields = [field("own"), field("inherited")];
        let args = [fields[0].1.clone(), fields[1].1.clone()];
        for depth in [0, 8, 64] {
            let ty = (0..depth).fold(Type::I64, |ty, _| Type::Array(Box::new(ty)));
            let mut hir = crate::ir::typed_ast::HirResolution::default();
            hir.classes.insert(
                base,
                HirClassInfo {
                    fields: vec![("inherited".into(), ty.clone())],
                    ..Default::default()
                },
            );
            hir.classes.insert(
                child,
                HirClassInfo {
                    base: Some(base),
                    fields: vec![("inherited".into(), Type::Bool), ("own".into(), ty)],
                    ..Default::default()
                },
            );
            let resolution = LoweringDeclarations::new(&hir);
            let callables = Callables::new(&resolution);
            REBUILT_TYPE_NODES.set(0);
            assert!(object_literal_supported(
                callables.resolution,
                &child,
                &fields
            ));
            assert!(constructor_supported(
                &child,
                &args,
                &callables,
                &mut ScalarPlan::default()
            ));
            assert!(!constructor_supported(
                &child,
                &args[..1],
                &callables,
                &mut ScalarPlan::default()
            ));
            let mut ordered: Vec<_> = resolution.fields_rev(&child).unwrap().collect();
            ordered.reverse();
            assert_eq!(ordered.len(), 2);
            assert!(std::ptr::eq(
                ordered[0],
                &callables.resolution.classes[&base].fields[0]
            ));
            assert!(std::ptr::eq(
                ordered[1],
                &callables.resolution.classes[&child].fields[1]
            ));
            assert_eq!(REBUILT_TYPE_NODES.get(), 0, "field type depth {depth}");
        }
    }

    #[test]
    fn constructor_shell_precedes_conditional_argument_cfg() {
        let program = program(
            "class Box { pub value: i64; } fn f(c: bool) -> Box { return new Box(c ? gc_allocated_bytes() : 0); }",
        );
        let function = &program.functions[0];
        assert!(function.blocks[0].instrs.iter().any(|inst| matches!(
            inst,
            SourceInst::Compute {
                value: LirRvalue::ObjectAlloc { .. },
                ..
            }
        )));
        assert!(matches!(
            function.blocks[0].terminator,
            SourceTerminator::Branch { .. }
        ));
        assert!(
            function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    SourceInst::Compute {
                        value: LirRvalue::FieldStore { .. },
                        ..
                    }
                ))
        );
    }

    #[test]
    fn enum_shell_precedes_payload_evaluation() {
        let program = program(
            "enum Pair { Values(i64, i64) } fn f() -> Pair { return Pair::Values(gc_allocated_bytes(), gc_allocated_bytes()); }",
        );
        let kinds: Vec<_> = program.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                SourceInst::Compute {
                    value: LirRvalue::EnumAlloc { .. },
                    ..
                } => Some("alloc"),
                SourceInst::Compute {
                    value: LirRvalue::EnumPayloadStore { .. },
                    ..
                } => Some("store"),
                SourceInst::Compute {
                    value: LirRvalue::BuiltinCall { .. },
                    ..
                } => Some("call"),
                // This assertion projects constructor effects in evaluation order.
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["alloc", "call", "store", "call", "store"]);
    }

    #[test]
    fn formatting_finishes_each_segment_before_next_operand() {
        let program = program(
            r#"fn f() -> String { return format("{}:{}", gc_allocated_bytes(), gc_allocated_bytes()); }"#,
        );
        // Every operand and literal segment is evaluated in source order.
        // String-concat fusion then formats the `{i64}`
        // segments straight into one sized result instead of allocating each
        // piece, so only the final concatenation follows the operands.
        let mut kinds = Vec::new();
        let mut segments = Vec::new();
        for inst in program.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
        {
            let SourceInst::Compute { local, value, .. } = inst else {
                continue;
            };
            match value {
                LirRvalue::BuiltinCall { .. } => {
                    kinds.push("call");
                    segments.push(*local);
                }
                LirRvalue::StringLiteral(text) => {
                    assert_eq!(text, ":");
                    kinds.push("literal");
                    segments.push(*local);
                }
                LirRvalue::FormatScalar { .. } => kinds.push("format"),
                LirRvalue::Binary {
                    operand_ty: Type::String,
                    ..
                } => kinds.push("concat"),
                LirRvalue::StringConcat { parts, part_types } => {
                    assert_eq!(part_types, &[Type::I64, Type::String, Type::I64]);
                    kinds.push("concat");
                    let parts: Vec<_> = parts
                        .iter()
                        .map(|part| match part {
                            LirOperand::Local(local) => *local,
                            other => panic!("unexpected part {other:?}"),
                        })
                        .collect();
                    assert_eq!(parts, segments, "segments keep source order");
                }
                // Only formatting and its inputs contribute events to this assertion.
                _ => {}
            }
        }
        assert_eq!(kinds, ["call", "literal", "call", "concat"]);
    }

    #[test]
    fn array_allocation_and_stores_interleave_element_evaluation() {
        let program = program(
            "fn f() -> Array<i64> { return [gc_allocated_bytes(), gc_allocated_bytes()]; }",
        );
        let values: Vec<_> = program.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                if let SourceInst::Compute { value, .. } = inst {
                    Some(value)
                } else {
                    None
                }
            })
            .collect();
        let kinds: Vec<_> = values
            .iter()
            .filter_map(|value| match value {
                LirRvalue::ArrayAlloc { .. } => Some("alloc"),
                LirRvalue::ArrayStore { .. } => Some("store"),
                LirRvalue::BuiltinCall { .. } => Some("call"),
                // This assertion projects array construction effects in evaluation order.
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["alloc", "call", "store", "call", "store"]);
        for inst in program.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
        {
            if let SourceInst::Compute { local, value, .. } = inst {
                assert!(
                    value.is_well_typed(&program.functions[0].locals, *local),
                    "{value:?}"
                );
            }
        }
    }
}
