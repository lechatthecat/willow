//! Operand-only LIR graph and operation validation.

use super::*;

/// Conservative "can evaluating this run a collection?" test, used to decide
/// whether a live GC temporary has to be rooted across a sub-expression. Only
/// the forms known to be allocation-free answer `false`; everything else —
/// including any expression form added later — answers `true`, so a new node
/// kind cannot silently drop a root.
#[cfg(test)]
pub(super) fn may_allocate(e: &HirExpr) -> bool {
    let mut pending = vec![e];
    while let Some(expr) = pending.pop() {
        match &expr.kind {
            HirExprKind::Int(_)
            | HirExprKind::Float(_)
            | HirExprKind::Bool(_)
            | HirExprKind::Var(_)
            | HirExprKind::FnRef(_) => {}
            HirExprKind::ReferenceArg { place } => pending.push(place),
            // Only constructing a captured environment allocates. Never inspect
            // a lambda body: it executes later as a separate callable.
            HirExprKind::Lambda { .. } => {
                if matches!(expr.ty, Type::Closure(..)) {
                    return true;
                }
            }
            HirExprKind::Unary { operand, .. } => pending.push(operand),
            HirExprKind::Binary { op, lhs, rhs } => {
                if lhs.ty == Type::String && matches!(op, BinOp::Add) {
                    return true;
                }
                pending.extend([rhs.as_ref(), lhs.as_ref()]);
            }
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => {
                pending.extend([else_expr.as_ref(), then_expr.as_ref(), condition.as_ref()]);
            }
            HirExprKind::Index { array, index } => {
                pending.extend([index.as_ref(), array.as_ref()]);
            }
            HirExprKind::FieldAccess { object, .. } => pending.push(object),
            // Unknown forms conservatively retain GC roots.
            _ => return true,
        }
    }
    false
}

/// Conservative eligibility: every type, instruction, and expression must be in
/// the supported subset, every callee must be a known symbol, every variable
/// must be a parameter or a `let` of this function, and binding names must be
/// unique across it (LIR flattens block scopes, so shadowing across sibling
/// scopes — or over a parameter — would alias one variable).
///
/// This is [`lir_rejection_reason`] with the reason discarded, so the decision
/// and the message it reports can never disagree. Codegen calls the reason form
/// directly (it has to print it), leaving this as the shorthand the eligibility
/// tests below are written against.
#[cfg(test)]
pub(in crate::backend::cranelift) fn lir_supported_function(
    f: &LirFunction,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    lir_rejection_reason(f, ctx).is_none()
}

/// Backend eligibility uses the same cleanup graph that drives emission.
pub(super) fn lir_cleanup_graph_is_valid(f: &LirFunction) -> bool {
    crate::ir::lowered::cleanup::CleanupGraph::lower(f).is_ok()
}

/// Why the walker will not compile `f`, phrased to read after "has invalid
/// lowered IR: " — `None` when the function IS in the subset.
///
/// The single source of truth for eligibility (see [`lir_supported_function`]).
/// This is what the compile error prints, which is the difference between
/// "something in this function is unsupported" and a construct, a type and a
/// line to go and fix.
#[allow(clippy::too_many_lines, reason = "split pending")]
pub(in crate::backend::cranelift) fn lir_rejection_reason(
    f: &LirFunction,
    ctx: &LirTypeCtx<'_>,
) -> Option<String> {
    if f.blocks.iter().enumerate().any(|(index, block)| {
        block.id.0 != index
            || lir_block_successors(block)
                .iter()
                .any(|target| *target >= f.blocks.len())
            || block
                .recovery
                .iter()
                .any(|target| target.0 >= f.blocks.len())
    }) {
        return Some("the LIR graph has an invalid block identity or target".into());
    }
    // Entry contains parameter/local initialization and is not a loop header.
    // Cleanup replay also suppresses its entry cancellation check so cleanup
    // can begin while cancellation is sticky; cycles need a separate header.
    if f.blocks.iter().any(|block| {
        lir_block_successors(block).contains(&0)
            || block.recovery.iter().any(|target| target.0 == 0)
    }) {
        return Some("the LIR entry block must not have incoming edges".into());
    }
    if lir_call_frame_entries(f).is_none() {
        return Some("method preparation frames disagree across control-flow edges".into());
    }
    if lir_reference_scope_entries(f).is_none() {
        return Some("reference argument scopes disagree across control-flow edges".into());
    }
    // The two per-function fields, taken from the function under test rather
    // than from the caller, so a `return` inside a `match` arm is checked
    // against this function's declared type and no caller can get it wrong.
    // The enclosing class comes from the lowered name (`Class::method`), which
    // is the same string `compile_class_method_inner` looks the body up under
    // and the same class the emitter sets as `FuncGen::current_class`.
    let owner = f.name.owner_type().map(|owner| owner.to_string());
    let ctx = &LirTypeCtx {
        #[cfg(test)]
        return_type: &f.return_type,
        // `rsplit`, not `split`: a module class is keyed by its qualified
        // name, so the method `shapes::Point::area` has `shapes::Point` as its
        // class and only the LAST separator divides the two.
        self_class: owner.as_deref().or_else(|| {
            if f.name.is_free_named("$defer") {
                ctx.self_class
            } else {
                None
            }
        }),
        ..*ctx
    };
    if !ctx.supported_type(&f.return_type) {
        return Some(format!(
            "its return type `{}` is outside the walker's subset",
            type_name(&f.return_type)
        ));
    }
    for p in &f.params {
        if !ctx.supported_type(&p.ty) {
            return Some(format!(
                "parameter `{}` has type `{}`, outside the walker's subset",
                p.name,
                type_name(&p.ty)
            ));
        }
    }
    // Names the walker can resolve, mapped to the type they are BOUND with (not
    // the initialiser's type — see `LirInst::Let::ty`). Any other `Var` is
    // something the HIR spells like a variable but codegen must special-case —
    // a bare enum variant, a function used as a value — so an unresolved
    // form fails validation.
    let mut names: HashMap<&str, Cow<'_, Type>> = HashMap::new();
    let mut physical_names = HashSet::new();
    for local in &f.locals {
        if !physical_names.insert(local.name.as_str()) {
            return Some(format!(
                "LIR local `{}` reuses an existing lowered name",
                local.name
            ));
        }
        if local.is_gc_owner() {
            if !local.synthetic || local.parameter || local.ty != Type::Void {
                return Some(
                    "opaque GC owners must be synthetic storage without a language type".into(),
                );
            }
        } else {
            names.insert(local.name.as_str(), Cow::Borrowed(&local.ty));
        }
    }

    // Opaque owners carry no language type. Recover their checked capture
    // provenance so malformed LIR cannot reinterpret a buffer or replace its
    // bounds-checked index with another local.
    let mut array_captures = HashMap::new();
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Compute {
                local,
                value: crate::ir::lowered::LirRvalue::CaptureArrayOwner { array, index },
                ..
            } = inst
            {
                let Some(Type::Array(ref element)) = array.ty(&f.locals) else {
                    return Some("array reference owner has no array provenance".into());
                };
                if array_captures
                    .insert(*local, ((**element).clone(), index.clone()))
                    .is_some()
                {
                    return Some("array reference owner has multiple definitions".into());
                }
            }
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Compute { value, .. } = inst {
                let mut operands = value.operands();
                if let crate::ir::lowered::LirRvalue::ReferenceDebug { argument, .. } = value {
                    operands.push(argument);
                }
                for operand in operands {
                    if let crate::ir::lowered::LirOperand::Reference {
                        place:
                            crate::ir::lowered::LirPlace::ArrayElement {
                                owner,
                                index,
                                element,
                            },
                        ..
                    } = operand
                        && !array_captures
                            .get(owner)
                            .is_some_and(|(captured, checked_index)| {
                                ctx.same_repr(captured, element)
                                    && *checked_index
                                        == crate::ir::lowered::LirOperand::Local(*index)
                            })
                    {
                        return Some(
                            "array reference disagrees with its captured buffer or checked index"
                                .into(),
                        );
                    }
                }
            }
        }
    }

    // A synchronous function's defer scopes live on the emitter's own Rust-side
    // stack, so the walker can only compile one whose open scopes are the same
    // whichever way control reached a block AND are what the emitter is holding
    // when it gets there.
    if !lir_cleanup_graph_is_valid(f) {
        return Some("its defer scope crosses LIR blocks".to_string());
    }

    // User binding types remain the primary diagnostic even after their
    // initializers have been split into preceding Compute instructions.
    for block in &f.blocks {
        for instruction in &block.instrs {
            if let LirInst::Let { name, ty, .. } = instruction
                && !ctx.supported_type(ty)
            {
                return Some(format!(
                    "`let {name}` binds type `{}`, outside the walker's subset",
                    type_name(ty)
                ));
            }
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            match inst {
                LirInst::Compute { local, value, span } => {
                    if let crate::ir::lowered::LirRvalue::DirectCall {
                        callee,
                        args,
                        params,
                        result,
                    } = value
                    {
                        if !(ctx.known_fn_id)(callee) {
                            return Some(format!(
                                "the call to `{callee}` at line {} has no declared function",
                                span.line
                            ));
                        }
                        if let Some(ty) = std::iter::once(result)
                            .chain(params)
                            .find(|ty| !ctx.supported_type(ty))
                        {
                            return Some(format!(
                                "the call to `{callee}` at line {} has type `{}`, outside the supported LIR types",
                                span.line,
                                type_name(ty)
                            ));
                        }
                        if !flat_argument_modes_match(
                            args,
                            ctx.func_param_modes
                                .get_id(callee)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                        ) {
                            return Some(format!(
                                "the call to `{callee}` has incompatible reference modes"
                            ));
                        }
                        if ctx.fn_types.get_id(callee).is_some_and(|signature| !matches!(signature,
                            Type::Fn(declared, output) if declared.len() == params.len()
                                && declared.iter().zip(params).all(|(declared, actual)| ctx.same_repr(declared, actual))
                                && ctx.same_repr(output, result)))
                             {
                            return Some(format!("the call to `{callee}` at line {} has an incompatible signature", span.line));
                        }
                    }
                    match value {
                        crate::ir::lowered::LirRvalue::FunctionRef { function, ty }
                            if !ctx
                                .fn_value_of(function)
                                .is_some_and(|known| ctx.same_callable(&known, ty)) =>
                        {
                            return Some(format!(
                                "the function value `{function}` at line {} has an incompatible signature",
                                span.line
                            ));
                        }
                        crate::ir::lowered::LirRvalue::Closure { id, ty, .. }
                            if !(ctx.lambda_symbol)(*id)
                                .and_then(|symbol| ctx.fn_value_of(&symbol))
                                .is_some_and(|known| ctx.same_callable(&known, ty)) =>
                        {
                            return Some(format!(
                                "a lambda at line {} has an unsupported environment or signature",
                                span.line
                            ));
                        }
                        _ => {}
                    }
                    if let crate::ir::lowered::LirRvalue::IntrinsicCall {
                        intrinsic,
                        receiver_ty,
                        arg_types,
                        result,
                        method,
                        ..
                    } = value
                        && !flat_intrinsic_supported(
                            *intrinsic,
                            receiver_ty,
                            arg_types,
                            result,
                            ctx,
                        )
                    {
                        return Some(format!(
                            "the `{method}` method at line {} uses an unsupported type",
                            span.line
                        ));
                    }
                    if !flat_rvalue_supported(value, &f.locals, ctx)
                        || !value.is_well_typed(&f.locals, *local)
                    {
                        use crate::ir::lowered::LirRvalue as V;
                        let operation = match value {
                            V::PrepareMethod {
                                receiver_ty,
                                method,
                                ..
                            }
                            | V::MethodCall {
                                receiver_ty,
                                method,
                                ..
                            }
                            | V::EnumMethod {
                                receiver_ty,
                                method,
                                ..
                            } => format!("the method `{method}` on a `{}`", type_name(receiver_ty)),
                            V::StaticField { class, field, .. }
                            | V::StaticStore { class, field, .. } => {
                                format!("the static property `{class}::{field}`")
                            }
                            V::ObjectAlloc { class } | V::ConstructorCall { class, .. } => {
                                format!("`new {class}`")
                            }
                            V::FieldLoad {
                                object_ty, field, ..
                            }
                            | V::FieldStore {
                                object_ty, field, ..
                            } => format!("the field `{field}` on a `{}`", type_name(object_ty)),
                            V::StaticCall { class, method, .. } => {
                                format!("the call to `{class}::{method}`")
                            }
                            V::StartTask { callee, .. } => {
                                format!("the task constructor `{callee}`")
                            }
                            _ => "a flat computation".into(),
                        };
                        return Some(format!(
                            "{operation} at line {} has incompatible operands, metadata or destination",
                            span.line
                        ));
                    }
                }
                LirInst::Let {
                    local,
                    name,
                    ty,
                    value,
                    ..
                } => {
                    if !f.locals.get(local.0 as usize).is_some_and(|slot| {
                        slot.name == *name && slot.ty == *ty && !slot.is_gc_owner()
                    }) || !value
                        .ty(&f.locals)
                        .is_some_and(|source| ctx.storable(ty, &source))
                        || matches!(value, crate::ir::lowered::LirOperand::Reference { .. })
                    {
                        return Some(format!(
                            "`let {name}` has incompatible LIR storage or operand"
                        ));
                    }
                }
                LirInst::Assign { local, name, value } => {
                    if !f.locals.get(local.0 as usize).is_some_and(|slot| {
                        slot.name == *name
                            && !slot.is_gc_owner()
                            && value
                                .ty(&f.locals)
                                .is_some_and(|source| ctx.storable(&slot.ty, &source))
                    }) || matches!(value, crate::ir::lowered::LirOperand::Reference { .. })
                    {
                        return Some(format!(
                            "`{name} = ...` has incompatible LIR storage or operand"
                        ));
                    }
                }
                LirInst::Unsupported { span, reason } => {
                    return Some(format!("{reason} at line {}", span.line));
                }
                // Listed rather than caught by `_` so a new instruction has to
                // be given an explicit validation decision here.
                LirInst::EnterDeferScope { .. } | LirInst::LeaveDeferScope { .. } => {}
                LirInst::FlushDefers { .. } => {}
                // Names locals this walker has already decided on, and stores
                // a constant into the slots it gave them.
                LirInst::ClearScopeRoots { .. } => {}
                // Every release has a matching acquisition in the same
                // function, and the decision is made there — on the terminator
                // that carries the type this reads back.
                LirInst::ReleaseLock { .. } => {}
                LirInst::Defer { body, .. } => {
                    let supported = lir_rejection_reason(&body.function, ctx).is_none();
                    if !supported {
                        return Some("it registers an unsupported `defer` body".to_string());
                    }
                }
                // A `match` lowering split into blocks.
                // The pattern is vetted here rather than through
                // `supported_expr`, because the arm bodies are already ordinary
                // LIR statements and their binding names are already in `names`
                // as locals of this function.
                LirInst::MatchTest {
                    scrutinee,
                    pattern,
                    span,
                    ..
                }
                | LirInst::MatchBind {
                    scrutinee,
                    pattern,
                    span,
                    ..
                } => {
                    let Some(scrutinee_local) = f
                        .locals
                        .get(scrutinee.0 as usize)
                        .filter(|local| !local.is_gc_owner())
                    else {
                        return Some("a match references an invalid scrutinee local".into());
                    };
                    let scrutinee_ty = &scrutinee_local.ty;
                    match inst {
                        LirInst::MatchTest { result, .. }
                            if !f.locals.get(result.0 as usize).is_some_and(|local| {
                                !local.is_gc_owner() && local.ty == Type::Bool
                            }) =>
                        {
                            return Some("a match test has an invalid boolean destination".into());
                        }
                        LirInst::MatchBind { bindings, .. }
                            if bindings.iter().any(|id| {
                                f.locals
                                    .get(id.0 as usize)
                                    .is_none_or(|local| local.is_gc_owner())
                            }) =>
                        {
                            return Some("a match binding references an invalid local".into());
                        }
                        _ => {}
                    }
                    let mut arm_names = names.clone();
                    if !supported_lir_pattern(pattern, scrutinee_ty, ctx, &mut arm_names) {
                        return Some(format!(
                            "the `match` arm at line {} tests a `{}` with a pattern outside the \
                             walker's subset",
                            span.line,
                            type_name(scrutinee_ty)
                        ));
                    }
                }
                LirInst::SelectInit { .. }
                | LirInst::SelectProbe { .. }
                | LirInst::SelectPick { .. }
                | LirInst::SelectUnregister { .. }
                | LirInst::SelectCommit { .. } => {}
            }
        }
        match &block.terminator {
            Terminator::Branch { cond, .. } => {
                if cond.ty(&f.locals) != Some(Type::Bool)
                    || matches!(cond, crate::ir::lowered::LirOperand::Reference { .. })
                {
                    return Some("a branch has an invalid boolean operand".into());
                }
            }
            Terminator::Return(Some(value)) => {
                if value.ty(&f.locals) == Some(Type::Void) {
                    return Some("the `return` yields a `void` value, which has no slot in the function's signature".into());
                }
                if !value
                    .ty(&f.locals)
                    .is_some_and(|ty| ty != Type::Void && ctx.storable(&f.return_type, &ty))
                    || matches!(value, crate::ir::lowered::LirOperand::Reference { .. })
                {
                    return Some("a return has an incompatible operand or signature".into());
                }
            }
            Terminator::Suspend { .. } if !f.is_async => {
                return Some("a synchronous function contains a suspension edge".to_string());
            }
            // A `lock` critical section. The protected value
            // is loaded into a frame slot on acquisition and committed back on
            // release, so the walker has to be able to represent it. The lock
            // HANDLE needs no check of its own: the lowerer hoisted it into a
            // `let`, which the instruction loop above already vetted.
            Terminator::Suspend {
                operation: SuspendOp::LockAcquire { slots, span },
                ..
            } => {
                let value_ty = &slots.value_ty;
                if !ctx.supported_type(value_ty) {
                    return Some(format!(
                        "the `lock` at line {} protects type `{}`, outside the walker's subset",
                        span.line,
                        type_name(value_ty)
                    ));
                }
            }
            Terminator::Jump(_)
            | Terminator::Suspend { .. }
            | Terminator::Return(None)
            | Terminator::CleanupReturn => {}
        }
    }
    None
}

/// Final LIR cannot embed expression-level suspension trees.
pub(in crate::backend::cranelift) fn lir_async_rejection_reason(f: &LirFunction) -> Option<String> {
    if let Some(reason) = crate::ir::lowered::async_liveness::stable_borrow_rejection(f) {
        return Some(reason);
    }
    f.blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .find_map(|inst| match inst {
            LirInst::Unsupported { span, reason } => {
                Some(format!("{reason} at line {}", span.line))
            }
            _ => None,
        })
}

/// Representation checks for resolved flat builtin methods. Receiver/result
/// identity and arity are checked against the semantic intrinsic table first.
pub(super) fn flat_intrinsic_supported(
    intrinsic: Intrinsic,
    receiver: &Type,
    args: &[Type],
    result: &Type,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    use Intrinsic::*;
    if !ctx.supported_type(receiver)
        || !ctx.supported_type(result)
        || !args.iter().all(|ty| ctx.supported_type(ty))
    {
        return false;
    }
    match intrinsic {
        ArrayResize => args == [Type::I64],
        ChannelSend => {
            let element = match receiver {
                Type::Array(element) => &**element,
                _ => match builtin_types::unary_arg(receiver, B::Channel) {
                    Some(element) => element,
                    None => return false,
                },
            };
            args.len() == 1 && ctx.storable(element, &args[0])
        }
        StringDebug => *receiver == Type::String && args.is_empty(),
        MapContains | FrozenMapContains | MapGet | FrozenMapGet | MapRemove | MapInsert
        | MapToString => {
            let Some((_, types)) = lir_collection(receiver) else {
                return false;
            };
            if types.len() != 2 {
                return false;
            }
            match intrinsic {
                MapToString => types.iter().all(|ty| collection_elem_kind(ty).is_some()),
                MapInsert => {
                    args.len() == 2
                        && ctx.same_repr(&types[0], &args[0])
                        && ctx.storable(&types[1], &args[1])
                }
                _ => args.len() == 1 && ctx.same_repr(&types[0], &args[0]),
            }
        }
        AtomicLoad | AtomicStore | AtomicSwap | AtomicCompareExchange | AtomicAdd | AtomicSub => {
            atomic_cell(receiver).is_some_and(|cell| args.iter().all(|arg| *arg == cell.word()))
        }
        CellGet | CellSet | RwCellRead | RwCellWrite => blocking_cell(receiver)
            .is_some_and(|(_, elem)| args.iter().all(|arg| ctx.same_repr(elem, arg))),
        _ => true,
    }
}

fn flat_static_call_supported(
    class: &str,
    method: &str,
    args: &[Type],
    result: &Type,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    let class = ctx.resolved_class(class);
    let builtin_class = B::from_name(class);
    if !ctx.supported_type(result) || args.iter().any(|ty| !ctx.supported_type(ty)) {
        return false;
    }
    if builtin_class == Some(B::Map) && method == "new" && args.is_empty() {
        return matches!(lir_collection(result), Some((LirCollection::Map, _)));
    }
    if builtin_class == Some(B::Channel) && builtin_types::unary_arg(result, B::Channel).is_some() {
        return (method == "new" && args.is_empty())
            || (method == "with_capacity" && args == [Type::I64]);
    }
    if let Some(cell) = atomic_cell(result)
        && builtin_class == Some(cell.type_id())
        && method == "new"
    {
        return args == [cell.word()];
    }
    if let Some((kind, elem)) = blocking_cell(result)
        && builtin_class == Some(kind.type_id())
        && method == "new"
    {
        return args.len() == 1 && ctx.same_repr(elem, &args[0]);
    }
    if let Some((_, elem)) = scheduler_lock(result)
        && matches!(builtin_class, Some(B::Mutex | B::RwLock))
        && method == "new"
    {
        return args.len() == 1 && ctx.same_repr(elem, &args[0]);
    }
    if let Some(handle) = cancellation_handle(result)
        && builtin_class == Some(handle.type_id())
        && method == "new"
    {
        return args.is_empty();
    }
    if let Some(entry) =
        namespace_builtin_call(ctx.known_modules, ctx.builtin_module_aliases, class, method)
    {
        return entry.params == args && ctx.repr_compatible(result, &entry.ret);
    }
    if ctx.is_enum(class) {
        let Some((name, definition)) = ctx.enum_instance(result) else {
            return false;
        };
        let Some(variant) = definition.variant(method) else {
            return false;
        };
        return name == TypeId::from_source_name(class)
            && variant.payloads.len() == args.len()
            && variant
                .payloads
                .iter()
                .zip(args)
                .all(|(slot, arg)| ctx.same_repr(slot, arg));
    }
    let (symbol, skip_self) = if let Some(prefix) = ctx.known_modules.linker_prefix(class) {
        (module_item_symbol(prefix, method), false)
    } else {
        if !ctx.supported_class(class) {
            return false;
        }
        (
            class_method_symbol_name(ctx.known_modules, class, method),
            true,
        )
    };
    if !(ctx.known_fn)(&symbol) {
        return false;
    }
    if ctx
        .func_param_modes
        .get(&symbol)
        .is_some_and(|modes| modes.iter().any(|mode| !matches!(mode, ParamMode::Value)))
    {
        return false;
    }
    let Some(Type::Fn(params, ret)) = ctx.fn_types.get(&symbol) else {
        return false;
    };
    let params = if skip_self {
        let Some((_, params)) = params.split_first() else {
            return false;
        };
        params
    } else {
        params.as_slice()
    };
    params.len() == args.len()
        && params
            .iter()
            .zip(args)
            .all(|(param, arg)| ctx.same_repr(param, arg))
        && ctx.same_repr(ret, result)
}

/// Validate metadata that operand type equality alone cannot establish.
pub(super) fn flat_rvalue_supported(
    value: &crate::ir::lowered::LirRvalue,
    locals: &[crate::ir::lowered::LirLocal],
    ctx: &LirTypeCtx<'_>,
) -> bool {
    use crate::ir::lowered::LirRvalue as V;
    let ty = |operand: &crate::ir::lowered::LirOperand| operand.ty(locals);
    let field_type = |object: &Type, field: &str| -> Option<Type> {
        if range_i64(object) {
            return matches!(field, "start" | "end").then_some(Type::I64);
        }
        ctx.class_layout_of(object)?
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, ty)| ty.clone())
    };
    if value.operands().iter().any(|operand| matches!(operand, crate::ir::lowered::LirOperand::Reference { place, .. } if !flat_reference_place_supported(place, locals, ctx))) { return false; }
    match value {
        V::ReferenceDebug { argument, .. } => matches!(argument, crate::ir::lowered::LirOperand::Reference { place, .. } if flat_reference_place_supported(place, locals, ctx)),
        V::StartTask { callee, args, params, output, .. } => ctx.cooperative_leaves.contains(&ctx.fn_types.scope().resolve(callee))
            && (ctx.known_fn_id)(callee)
            && ctx.supported_type(output) && params.iter().all(|ty| ctx.supported_type(ty))
            && ctx.fn_types.get_id(callee).is_some_and(|signature| matches!(signature, Type::Fn(declared, result) if declared.len() == params.len() && declared.iter().zip(params).all(|(expected, actual)| ctx.same_repr(expected, actual)) && builtin_types::unary_arg(result, B::Task).is_some_and(|payload| ctx.same_repr(payload, output))))
            && flat_argument_modes_match(args, ctx.func_param_modes.get_id(callee).map(Vec::as_slice).unwrap_or(&[])),
        V::AwaitFuture { result, .. } => ctx.supported_type(result),
        V::SelectIdleWait { .. } => true,
        V::PrepareMethod { receiver_ty, method, .. } => matches!(receiver_ty, Type::Array(_)) || flat_method_signature(receiver_ty, method, ctx).is_some(),
        V::MethodCall { receiver_ty, method, args, arg_types, result, super_class, .. } => {
            let dispatch = super_class.map(Type::Named);
            let receiver_ty = dispatch.as_ref().unwrap_or(receiver_ty);
            flat_argument_modes_match(args, &flat_method_modes(receiver_ty, method, ctx)) && flat_method_signature(receiver_ty, method, ctx).is_some_and(|(params, ret)| params.len() == arg_types.len() && params.iter().zip(arg_types).all(|(param, arg)| ctx.same_repr(param, arg)) && ctx.same_repr(&ret, result))
        },
        V::IndirectCall { callee, params, result, .. } => ty(callee).is_some_and(|callee| ctx.supported_type(&callee)) && params.iter().all(|param| ctx.supported_type(param)) && ctx.supported_type(result),
        V::RebindResultError { source, target, .. } => ctx.supported_enum_type(source) && ctx.supported_enum_type(target)
            && builtin_types::binary_args(source, B::Result).zip(builtin_types::binary_args(target, B::Result)).is_some_and(|((_, source), (_, target))| ctx.same_repr(source, target)),
        V::IntoError { source, target, .. } => {
            let Type::Named(class) = source else { return false; };
            ctx.supported_class(class) && ctx.supported_type(target) && ctx.resolve_class_method(class, "into").is_some_and(|symbol|
                ctx.fn_types.get(&symbol).is_some_and(|signature| matches!(signature, Type::Fn(params, result) if params.len() == 1 && ctx.same_repr(result, target))))
        }
        V::Coerce { source, target, .. } => ctx.supported_type(source) && ctx.supported_type(target) && ctx.storable(target, source),
        V::CaptureArrayOwner { array, index } => ty(array).is_some_and(|array| matches!(array, Type::Array(_)) && ctx.supported_type(&array)) && ty(index) == Some(Type::I64),
        V::ArrayAlloc { length, element } => *length <= i64::MAX as usize && ctx.supported_type(&Type::Array(Box::new(element.clone()))),
        V::ArrayStore { array, value, element, .. } => ty(array).is_some_and(|array| ctx.supported_type(&array) && matches!(&array, Type::Array(inner) if ctx.same_repr(inner, element))) && ctx.supported_type(element) && ty(value).is_some_and(|value| ctx.storable(element, &value)),
        V::Index { array, element, .. } => ctx.supported_type(element) && ty(array).is_some_and(|array| ctx.supported_type(&array) && (matches!(&array, Type::Array(_)) || matches!(lir_collection(&array), Some((LirCollection::FrozenArray, _)))) && ctx.same_repr(&array_element_type(&array), element)),
        V::ObjectAlloc { class } => ctx.supported_class(class) && ctx.classes.is_class(class) && ctx.classes.class_type_id(class).is_some(),
        V::FieldLoad { object_ty, field, result, .. } => ctx.supported_type(object_ty) && ctx.supported_type(result) && field_type(object_ty, field).is_some_and(|field| ctx.same_repr(&field, result)),
        V::FieldStore { object_ty, field, value, .. } => ctx.class_layout_of(object_ty).is_some() && field_type(object_ty, field).is_some_and(|field| ty(value).is_some_and(|value| ctx.supported_type(&value) && ctx.storable(&field, &value))),
        V::StaticField { class, field, result } => (ctx.static_field)(ctx.resolved_class(&class.to_string()), field).is_some_and(|field| ctx.supported_type(&field) && ctx.same_repr(&field, result)),
        V::StaticStore { class, field, value } => (ctx.static_field)(ctx.resolved_class(&class.to_string()), field).is_some_and(|field| ctx.supported_type(&field) && ty(value).is_some_and(|value| ctx.supported_type(&value) && ctx.storable(&field, &value))),
        V::ConstructorCall { class, object, args, arg_types } => {
            if !ctx.supported_class(class) || !ty(object).is_some_and(|ty| matches!(&ty, Type::Named(name) if name == class)) || args.len() != arg_types.len() { return false; }
            let symbol = class_method_symbol_name(ctx.known_modules, &class.to_string(), "init");
            if !(ctx.known_fn)(&symbol) || !flat_argument_modes_match(args, ctx.func_param_modes.get(&symbol).map(Vec::as_slice).unwrap_or(&[])) { return false; }
            let Some(Type::Fn(params, result)) = ctx.fn_types.get(&symbol) else { return false; };
            let Some((_, params)) = params.split_first() else { return false; };
            **result == Type::Void && params.len() == arg_types.len() && params.iter().zip(arg_types).all(|(param, arg)| ctx.supported_type(param) && ctx.same_repr(param, arg))
        }
        V::StaticCall { class, method, args, arg_types, result, .. } => {
            if args.iter().any(|arg| matches!(arg, crate::ir::lowered::LirOperand::Reference { .. })) { flat_static_reference_call_supported(&class.to_string(), method, args, arg_types, result, ctx) }
            else { flat_static_call_supported(&class.to_string(), method, arg_types, result, ctx) }
        },
        V::EnumAlloc { class, variant, enum_ty } => ctx.supported_enum_type(enum_ty) && ctx.enum_instance(enum_ty).is_some_and(|(name, definition)| name == *class && definition.variant(variant).is_some()),
        V::EnumPayloadStore { class, variant, index, source, enum_ty, .. } => ctx.supported_enum_type(enum_ty) && ctx.supported_type(source) && ctx.enum_instance(enum_ty).is_some_and(|(name, definition)| name == *class && definition.variant(variant).and_then(|variant| variant.payloads.get(*index)).is_some_and(|slot| ctx.storable(slot, source))),
        V::Range { start, end } => ty(start) == Some(Type::I64) && ty(end) == Some(Type::I64),
        V::EnumMethod { receiver_ty, method, arg_types, result, .. } => ctx.supported_enum_type(receiver_ty) && ctx.supported_type(result) && arg_types.iter().all(|ty| ctx.supported_type(ty)) && option_result_method(receiver_ty, method, arg_types).is_some_and(|ret| ctx.same_repr(&ret, result)),
        V::BuiltinCall { callee, params, result, .. } => {
            if callee.is_free_named("$contract_i64") { return params == &[Type::I64, Type::I64, Type::I64] && *result == Type::I64; }
            if callee.is_free_named("$contract_check") { return params == &[Type::Bool, Type::I64] && *result == Type::Void; }
            let void_future = *result == Type::Void || builtin_types::unary_arg(result, B::Future) == Some(&Type::Void);
            if callee.is_free_named("sleep") { return params == &[Type::I64] && void_future; }
            if callee.is_free_named("yield") { return params.is_empty() && void_future; }
            if !params.is_empty() { return false; }
            if callee.is_free_named("gc_collect") || callee.is_free_named("gc_minor_collect") || callee.is_free_named("gc_task_frame_diagnostics") { *result == Type::Void }
            else { callee.is_free_named(callee.unqualified_name().as_deref().unwrap_or("")) && runtime_stat_builtin_runtime_name(callee.unqualified_name().as_deref().unwrap_or("")).is_some() && *result == Type::I64 }
        }
        V::FormatScalar { ty, format, .. } => matches!(ty, Type::I64 | Type::F64 | Type::Bool | Type::String) && (format.is_none() || *ty == Type::F64),
        V::Panic { message } => ty(message) == Some(Type::String),
        // A payload-free enum compares its tag; a class or interface value
        // compares its object word by reference identity.
        V::Binary { op, operand_ty, .. } if matches!(operand_ty, Type::Named(_) | Type::Generic(..)) => matches!(op, BinOp::Eq | BinOp::Ne) && (ctx.tag_immediate_enum(operand_ty) || (ctx.supported_type(operand_ty) && matches!(operand_ty, Type::Named(name) | Type::Generic(name, _) if ctx.is_class(name) || (ctx.is_interface)(name)))),
        V::Recover => true,
        _ => true,
    }
}

/// The by-value dispatch ABI, including inherited methods and interface Self.
pub(super) fn flat_method_signature(
    receiver: &Type,
    method: &str,
    ctx: &LirTypeCtx<'_>,
) -> Option<(Vec<Type>, Type)> {
    if !ctx.supported_type(receiver) {
        return None;
    }
    let name = match receiver {
        Type::Named(name) | Type::Generic(name, _) => name,
        _ => return None,
    };
    let (params, ret) = if (ctx.is_interface)(name) {
        let sig = (ctx.iface_method)(receiver, method)?;
        let ret = if matches!(&sig.ret, Type::Named(name) if *name == TypeId::local("Self")) {
            receiver.clone()
        } else {
            sig.ret
        };
        (sig.params, ret)
    } else {
        if !ctx.supported_class(name) {
            return None;
        }
        let symbol = ctx.resolve_class_method(name, method)?;
        let Type::Fn(params, ret) = ctx.fn_types.get(&symbol)? else {
            return None;
        };
        let (_, params) = params.split_first()?;
        (params.to_vec(), (**ret).clone())
    };
    (ctx.supported_type(&ret) && params.iter().all(|param| ctx.supported_type(param)))
        .then_some((params, ret))
}

fn flat_argument_modes_match(args: &[crate::ir::lowered::LirOperand], modes: &[ParamMode]) -> bool {
    args.iter().enumerate().all(|(index, arg)| {
        matches!(arg, crate::ir::lowered::LirOperand::Reference { .. })
            == matches!(modes.get(index), Some(ParamMode::Reference { .. }))
    })
}
fn flat_method_modes(receiver: &Type, method: &str, ctx: &LirTypeCtx<'_>) -> Vec<ParamMode> {
    let name = match receiver {
        Type::Named(name) | Type::Generic(name, _) => name,
        _ => return vec![],
    };
    if (ctx.is_interface)(name) {
        return (ctx.iface_method)(receiver, method)
            .map(|sig| sig.modes)
            .unwrap_or_default();
    }
    ctx.resolve_class_method(name, method)
        .and_then(|symbol| ctx.func_param_modes.get(&symbol).cloned())
        .unwrap_or_default()
}
fn flat_reference_place_supported(
    place: &crate::ir::lowered::LirPlace,
    locals: &[crate::ir::lowered::LirLocal],
    ctx: &LirTypeCtx<'_>,
) -> bool {
    use crate::ir::lowered::LirPlace;
    let Some(ty) = place.ty(locals) else {
        return false;
    };
    if !ctx.supported_type(&ty) || matches!(ty, Type::Void | Type::Never) {
        return false;
    }
    match place {
        LirPlace::Local(_) | LirPlace::ArrayElement { .. } => true,
        LirPlace::Field {
            object_ty, field, ..
        } => ctx.class_layout_of(object_ty).is_some_and(|layout| {
            layout
                .iter()
                .any(|(name, field_ty)| name == field && ctx.same_repr(field_ty, &ty))
        }),
    }
}
fn flat_static_reference_call_supported(
    class: &str,
    method: &str,
    args: &[crate::ir::lowered::LirOperand],
    types: &[Type],
    result: &Type,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    let class = ctx.resolved_class(class);
    let (symbol, skip_self) = if let Some(prefix) = ctx.known_modules.linker_prefix(class) {
        (module_item_symbol(prefix, method), false)
    } else {
        if !ctx.supported_class(class) {
            return false;
        }
        (
            class_method_symbol_name(ctx.known_modules, class, method),
            true,
        )
    };
    if !(ctx.known_fn)(&symbol)
        || !flat_argument_modes_match(
            args,
            ctx.func_param_modes
                .get(&symbol)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    {
        return false;
    }
    let Some(Type::Fn(params, ret)) = ctx.fn_types.get(&symbol) else {
        return false;
    };
    let params = if skip_self {
        let Some((_, tail)) = params.split_first() else {
            return false;
        };
        tail
    } else {
        params.as_slice()
    };
    params.len() == types.len()
        && params
            .iter()
            .zip(types)
            .all(|(param, arg)| ctx.supported_type(param) && ctx.same_repr(param, arg))
        && ctx.same_repr(ret, result)
}
