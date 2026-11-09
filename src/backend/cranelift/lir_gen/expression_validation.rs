//! HIR eligibility helpers and test-only expression validation.

use super::*;

/// Does this expression suspend the running task where it stands? `await`,
/// `select`, and a channel `send`/`recv` all return control to the scheduler.
/// A lambda body is a separate function, so its contents do not count.
#[cfg(test)]
fn lir_suspends_here(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Await { inner }
            if builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                && !matches!(&inner.kind, HirExprKind::Call { callee, .. } if matches!(callee.unqualified_name().as_deref().unwrap_or(""), "sleep" | "yield")) =>
        {
            false
        }
        HirExprKind::Await { .. } | HirExprKind::Select { .. } => true,
        HirExprKind::MethodCall { object, method, .. } => {
            builtin_types::unary_arg(&object.ty, B::Channel).is_some()
                && matches!(method.as_str(), "send" | "recv" | "recv_opt")
        }
        _ => false,
    }
}

/// Every suspension point inside `expr`, in evaluation order. An `await`'s own
/// operands are walked too: `await f(ch.recv)` holds two.
#[cfg(test)]
pub(super) fn lir_collect_suspensions<'e>(expr: &'e HirExpr, out: &mut Vec<&'e HirExpr>) {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        if lir_suspends_here(expr) {
            out.push(expr);
        }
        if !matches!(expr.kind, HirExprKind::Lambda { .. }) {
            pending.extend(expr.children().into_iter().rev());
        }
    }
}

/// Whether `expr` suspends anywhere inside it.
#[cfg(test)]
fn lir_expr_suspends(expr: &HirExpr) -> bool {
    let mut found = Vec::new();
    lir_collect_suspensions(expr, &mut found);
    !found.is_empty()
}

/// Values the emitter can produce twice with the same result, so evaluating
/// them AFTER a park that the source ran them before is not observable. This is
/// deliberately the same set the former AST suspension normalizer's `bind` refuses to hoist into
/// a temp, which is what keeps that pass and this one evaluating the same
/// things in the same order.
#[cfg(test)]
pub(super) fn lir_rematerializable(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::Int(_)
            | HirExprKind::Float(_)
            | HirExprKind::Bool(_)
            | HirExprKind::Str(_)
            | HirExprKind::Var(_)
    )
}

/// Forms whose children the emitter may evaluate zero times or on only one
/// path. A suspension underneath one of these would park on a path the source
/// never takes, so they end the search.
#[cfg(test)]
fn lir_evaluates_children_conditionally(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Ternary { .. }
        | HirExprKind::Match { .. }
        | HirExprKind::Select { .. }
        | HirExprKind::Lambda { .. } => true,
        HirExprKind::Binary { op, .. } => matches!(op, BinOp::And | BinOp::Or),
        _ => false,
    }
}

/// Whether `node` still evaluates correctly once `target` is pulled out in
/// front of it. `seen` tracks whether the walk has passed `target` yet: what
/// comes after it is emitted on the resume path and may be anything, while what
/// comes before it has to survive being moved after the park.
#[cfg(test)]
pub(super) fn lir_hoistable_around(node: &HirExpr, target: &HirExpr, seen: &mut bool) -> bool {
    let Some(path) = node.path_to(target) else {
        return *seen || lir_rematerializable(node);
    };
    let ancestors: HashSet<*const HirExpr> = path.into_iter().map(std::ptr::from_ref).collect();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if std::ptr::eq(node, target) {
            *seen = true;
            continue;
        }
        if !ancestors.contains(&std::ptr::from_ref(node)) {
            if !*seen && !lir_rematerializable(node) {
                return false;
            }
            continue;
        }
        if lir_evaluates_children_conditionally(node) {
            return false;
        }
        pending.extend(node.children().into_iter().rev());
    }
    true
}

/// An `await` the cooperative LIR path can split into a suspension
///. Classified once, by the function both eligibility and
/// emission call, so an admitted await is always an emittable one.
#[cfg(test)]
pub(super) enum LirAwaitSite<'a> {
    /// `await sleep(millis)`: a timer registration, no awaited frame.
    Sleep(&'a HirExpr),
    /// `await yield`: an unconditional reschedule.
    Yield,
    /// `await f(..)` where `f` is a cooperative leaf. The callee's constructor
    /// runs here and hands back the frame this task waits on.
    LeafCall {
        callee: &'a FunctionId,
        args: &'a [HirExpr],
    },
}

/// The await forms this splitter takes — the ones that are still ordinary
/// expressions when the walker reaches them. `await <task value>` and
/// `await t.result` are absent because lowering already turned them into a
/// [`Terminator::Suspend`] carrying [`crate::ir::lowered::SuspendOp::AwaitTask`],
/// which delivers its result into a LIR local, so nothing is left inside the
/// expression for this to find.
#[cfg(test)]
fn lir_await_site<'e>(
    expr: &'e HirExpr,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
) -> Option<LirAwaitSite<'e>> {
    if let Some(builtin) = lir_builtin_await(expr) {
        return Some(builtin);
    }
    let HirExprKind::Await { inner } = &expr.kind else {
        return None;
    };
    let HirExprKind::Call { callee, args } = &inner.kind else {
        return None;
    };
    if !cooperative_leaves.contains(&callee.clone()) {
        return None;
    }
    Some(LirAwaitSite::LeafCall { callee, args })
}

/// Recognise the two scheduler builtins whose await result is `void`. Keeping
/// this structural predicate beside LIR eligibility/emission gives both sides
/// exactly the same accepted shape.
#[cfg(test)]
fn lir_builtin_await(expr: &HirExpr) -> Option<LirAwaitSite<'_>> {
    let HirExprKind::Await { inner } = &expr.kind else {
        return None;
    };
    let HirExprKind::Call { callee, args } = &inner.kind else {
        return None;
    };
    if expr.ty != Type::Void
        || !builtin_types::unary_arg(&inner.ty, B::Future)
            .is_some_and(|output| *output == Type::Void)
    {
        return None;
    }
    match (
        callee.unqualified_name().as_deref().unwrap_or(""),
        args.as_slice(),
    ) {
        ("sleep", [millis]) if millis.ty == Type::I64 => Some(LirAwaitSite::Sleep(millis)),
        ("yield", []) => Some(LirAwaitSite::Yield),
        _ => None,
    }
}

#[cfg(test)]
pub(super) fn expr_rejection<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> Option<String> {
    let node = minimal_unsupported_expr(e, ctx, names)?;
    let what = describe_expr(node);
    let line = node.span.line;
    // `is_fresh_empty_map` is the one node whose own type is unsupported yet
    // which the walker still emits, so blaming its type would be wrong.
    if !ctx.supported_type(&node.ty) && !is_fresh_empty_map(node) {
        Some(format!(
            "{what} at line {line} has type `{}`, outside the walker's subset",
            type_name(&node.ty)
        ))
    } else {
        Some(format!(
            "{what} at line {line} is outside the walker's subset"
        ))
    }
}

/// The deepest sub-expression of `e` that `supported_expr` rejects, or `None`
/// if it accepts `e`. Descending stops at the two kinds that carry their own
/// binding scope: inside a lambda body, or inside an arm whose pattern the
/// walker cannot bind, `names` does not describe what is actually in scope, so
/// a child would be blamed for a name the walker simply never sees.
#[willow_continuations::lir_validation]
#[cfg(test)]
pub(super) fn minimal_unsupported_expr<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> Option<&'e HirExpr> {
    validate_expr(e, ctx, names, true).1
}

/// Diagnose a rejected scope boundary using the bindings visible inside it.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn minimal_unsupported_scoped_expr<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> Option<&'e HirExpr> {
    match &e.kind {
        HirExprKind::Lambda { .. } => Some(e),
        HirExprKind::Match { scrutinee, arms } => {
            if let Some(inner) = minimal_unsupported_expr(scrutinee, ctx, names) {
                return Some(inner);
            }
            for arm in arms {
                let mut arm_names = names.clone();
                if !supported_pattern(&arm.pattern, &scrutinee.ty, ctx, &mut arm_names) {
                    continue;
                }
                if let Some(guard) = &arm.guard
                    && let Some(inner) = minimal_unsupported_expr(guard, ctx, &arm_names)
                {
                    return Some(inner);
                }
                // A diverging arm is admissible on its own terms, so it must
                // not be reported as the reason a match around it failed.
                if arm_diverges(arm)
                    && supported_divergent_body(&arm.body, ctx, &arm_names, BodyScope::Bracketed)
                {
                    continue;
                }
                for child in arm.body.iter().flat_map(HirStmt::child_exprs) {
                    if let Some(inner) = minimal_unsupported_expr(child, ctx, &arm_names) {
                        return Some(inner);
                    }
                }
            }
            Some(e)
        }
        _ => {
            for child in e.children() {
                if let Some(rejected) = minimal_unsupported_expr(child, ctx, names) {
                    return Some(rejected);
                }
            }
            Some(e)
        }
    }
}

/// How to name an expression in a LIR validation diagnostic. Deliberately short: the line
/// number locates it, this says what to look for on that line.
#[cfg(test)]
fn describe_expr(e: &HirExpr) -> String {
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => {
            "a literal".to_string()
        }
        HirExprKind::Str(_) => "a string literal".to_string(),
        HirExprKind::Var(name) => format!("the variable `{name}`"),
        HirExprKind::FnRef(name) => format!("the function value `{name}`"),
        HirExprKind::Binary { op, .. } => format!("the `{}` operator", binop_str(op)),
        HirExprKind::Unary { .. } => "a unary operator".to_string(),
        HirExprKind::Invoke { .. } => "a computed callable invocation".to_string(),
        HirExprKind::Call { callee, .. } => format!("the call to `{callee}`"),
        HirExprKind::Print { .. } => "a `print`".to_string(),
        HirExprKind::Array { .. } => "an array literal".to_string(),
        HirExprKind::Index { .. } => "an index read".to_string(),
        HirExprKind::Ternary { .. } => "a `?:` expression".to_string(),
        HirExprKind::New { class, .. } => format!("`new {class}`"),
        HirExprKind::FieldAccess { object, field } => {
            format!("the field read `.{field}` on a `{}`", type_name(&object.ty))
        }
        HirExprKind::MethodCall { object, method, .. } => {
            format!("the method `{method}` on a `{}`", type_name(&object.ty))
        }
        HirExprKind::ObjectLiteral { class, .. } => {
            format!("the object literal `{class} {{ .. }}`")
        }
        HirExprKind::StaticField { class, field } => {
            format!("the static property `{class}::{field}`")
        }
        HirExprKind::StaticCall { class, method, .. } => {
            format!("the static call `{class}::{method}`")
        }
        HirExprKind::ReferenceArg { .. } => "a reference argument".to_string(),
        HirExprKind::Range { .. } => "a range".to_string(),
        HirExprKind::Await { .. } => "an `await`".to_string(),
        HirExprKind::TryPropagate { .. } => "a `?` propagation".to_string(),
        HirExprKind::Lambda { .. } => "a lambda".to_string(),
        HirExprKind::Match { scrutinee, .. } => {
            format!("the `match` on a `{}`", type_name(&scrutinee.ty))
        }
        HirExprKind::Select { .. } => "a `select`".to_string(),
    }
}

/// Whether the walker can both TEST `pattern` against a `scrutinee_ty` value
/// and bind whatever it destructures. On success the pattern's
/// bindings are added to `names`, which is why this takes the map by `&mut`:
/// the arm body is checked against the extended scope, exactly as the emitter
/// will run it. On failure `names` may have been partly extended, so callers
/// pass a throwaway clone rather than their own map.
///
/// `ClassDowncast` tests the boxed object's runtime `type_id`, so it is
/// admitted only for a class the walker has a `type_id` and a layout for
///.
pub(super) fn supported_lir_pattern<'n>(
    pattern: &'n crate::ir::lowered::LirPattern,
    scrutinee_ty: &Type,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    // The variant lookup every enum pattern needs: the scrutinee must BE this
    // enum (a pattern naming another enum is a checker bug, not something to
    // emit a tag compare for) and the enum must be one the walker admits.
    let variant_of = |enum_name: &str, variant: &str| {
        if !ctx.supported_enum_type(scrutinee_ty) {
            return None;
        }
        // The payloads come from the SCRUTINEE's type, so a generic enum's
        // placeholders are already resolved to the types this `match` will
        // actually load.
        let (name, def) = ctx.enum_instance(scrutinee_ty)?;
        if name != TypeId::from_source_name(enum_name) {
            return None;
        }
        def.variant(variant).cloned()
    };
    match pattern {
        crate::ir::lowered::LirPattern::Wildcard => true,
        crate::ir::lowered::LirPattern::Binding { name, ty } => {
            // The binding aliases the whole scrutinee, so it must hold the same
            // machine representation — no widening, no boxing.
            if !ctx.same_repr(ty, scrutinee_ty) || !ctx.supported_type(ty) {
                return false;
            }
            names.insert(name.as_str(), Cow::Borrowed(ty));
            true
        }
        crate::ir::lowered::LirPattern::LiteralBool(_) => *scrutinee_ty == Type::Bool,
        crate::ir::lowered::LirPattern::LiteralInt(_) => *scrutinee_ty == Type::I64,
        crate::ir::lowered::LirPattern::EnumVariant { enum_name, variant } => {
            variant_of(&enum_name.to_string(), variant).is_some_and(|v| v.payloads.is_empty())
        }
        crate::ir::lowered::LirPattern::EnumVariantTuple {
            enum_name,
            variant,
            bindings,
        } => {
            let Some(v) = variant_of(&enum_name.to_string(), variant) else {
                return false;
            };
            // A variant whose payloads are ALL `void` carries no word at all:
            // `enum_instance` normalizes the list away, because there is
            // nothing to store. The source still spells one binding per
            // declared payload — `Ok(done)` on a `Result<void, Cancelled>`,
            // which is what `TaskScope::finish` produces — so the counts differ
            // by exactly that. The emitter zips bindings
            // against the same normalized list and so binds nothing here, which
            // is correct: the name denotes a value that does not exist. An arm
            // body that reads it finds no binding and fails LIR validation
            // rather than loading a word that was never written.
            if !bindings.is_empty()
                && v.payloads.is_empty()
                && bindings.iter().all(|(_, ty)| matches!(ty, Type::Void))
            {
                return true;
            }
            if v.payloads.len() != bindings.len() {
                return false;
            }
            // A payload is LOADED into the binding, so the binding's type has
            // to match the slot's representation rather than merely be
            // storable into it.
            for (slot, (name, ty)) in v.payloads.iter().zip(bindings) {
                if !ctx.same_repr(ty, slot) || !ctx.supported_type(ty) {
                    return false;
                }
                names.insert(name.as_str(), Cow::Borrowed(ty));
            }
            true
        }
        // `Iface(x)` — an interface-to-class downcast. The scrutinee is a box
        // whose word 0 is the concrete object, so the test is an exact
        // `type_id` compare and the binding is that object, unboxed. Exact, not
        // "is a descendant of": a downcast arm selects the arm's class itself,
        // so a descendant of it does not match.
        crate::ir::lowered::LirPattern::ClassDowncast {
            class_name,
            binding,
            binding_ty,
        } => {
            // `Box<i64>` as well as `Shape`: the type arguments name what the
            // interface's methods produce, and a `type_id` compare against
            // word 0 does not read them.
            let (Type::Named(iface) | Type::Generic(iface, _)) = scrutinee_ty else {
                return false;
            };
            if !(ctx.is_interface)(iface)
                || !matches!(binding_ty, Type::Named(n) if n == class_name)
                || !ctx.supported_class(&class_name.to_string())
                || ctx.classes.class_type_id(class_name).is_none()
            {
                return false;
            }
            names.insert(binding.as_str(), Cow::Borrowed(binding_ty));
            true
        }
    }
}

/// The result type of a builtin `Option`/`Result` method the walker emits, or
/// `None` when the receiver/method pair is outside the subset.
///
/// A `void` payload is excluded throughout because the unwrap family would read
/// a payload slot that an all-`void` variant does not allocate, and the
/// callable-taking combinators would build one.
///
/// The combinators (`map`/`map_err`/`and_then`/`or_else`) came with function
/// values in. Each rule below reconstructs the type the SHARED
/// emitter actually builds; the caller then compares it against the checker's
/// own type for the expression, so a disagreement refuses the body rather
/// than being reinterpreted here.
///
/// `args` are the call's argument types, so arity and the default/message
/// operand are vetted here rather than at two call sites.
/// The return type of a `fn(...) -> R` value. Eligibility has already proved
/// every combinator operand is one, so a non-function here is a compiler bug.
#[cfg(test)]
fn supported_pattern<'n>(
    pattern: &'n HirPattern,
    scrutinee_ty: &Type,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    // The variant lookup every enum pattern needs: the scrutinee must BE this
    // enum (a pattern naming another enum is a checker bug, not something to
    // emit a tag compare for) and the enum must be one the walker admits.
    let variant_of = |enum_name: &str, variant: &str| {
        if !ctx.supported_enum_type(scrutinee_ty) {
            return None;
        }
        // The payloads come from the SCRUTINEE's type, so a generic enum's
        // placeholders are already resolved to the types this `match` will
        // actually load.
        let (name, def) = ctx.enum_instance(scrutinee_ty)?;
        if name != TypeId::from_source_name(enum_name) {
            return None;
        }
        def.variant(variant).cloned()
    };
    match pattern {
        HirPattern::Wildcard => true,
        HirPattern::Binding { name, ty } => {
            // The binding aliases the whole scrutinee, so it must hold the same
            // machine representation — no widening, no boxing.
            if !ctx.same_repr(ty, scrutinee_ty) || !ctx.supported_type(ty) {
                return false;
            }
            names.insert(name.as_str(), Cow::Borrowed(ty));
            true
        }
        HirPattern::LiteralBool(_) => *scrutinee_ty == Type::Bool,
        HirPattern::LiteralInt(_) => *scrutinee_ty == Type::I64,
        HirPattern::EnumVariant { enum_name, variant } => {
            variant_of(&enum_name.to_string(), variant).is_some_and(|v| v.payloads.is_empty())
        }
        HirPattern::EnumVariantTuple {
            enum_name,
            variant,
            bindings,
        } => {
            let Some(v) = variant_of(&enum_name.to_string(), variant) else {
                return false;
            };
            // A variant whose payloads are ALL `void` carries no word at all:
            // `enum_instance` normalizes the list away, because there is
            // nothing to store. The source still spells one binding per
            // declared payload — `Ok(done)` on a `Result<void, Cancelled>`,
            // which is what `TaskScope::finish` produces — so the counts differ
            // by exactly that. The emitter zips bindings
            // against the same normalized list and so binds nothing here, which
            // is correct: the name denotes a value that does not exist. An arm
            // body that reads it finds no binding and fails LIR validation
            // rather than loading a word that was never written.
            if !bindings.is_empty()
                && v.payloads.is_empty()
                && bindings.iter().all(|(_, ty)| matches!(ty, Type::Void))
            {
                return true;
            }
            if v.payloads.len() != bindings.len() {
                return false;
            }
            // A payload is LOADED into the binding, so the binding's type has
            // to match the slot's representation rather than merely be
            // storable into it.
            for (slot, (name, ty)) in v.payloads.iter().zip(bindings) {
                if !ctx.same_repr(ty, slot) || !ctx.supported_type(ty) {
                    return false;
                }
                names.insert(name.as_str(), Cow::Borrowed(ty));
            }
            true
        }
        // `Iface(x)` — an interface-to-class downcast. The scrutinee is a box
        // whose word 0 is the concrete object, so the test is an exact
        // `type_id` compare and the binding is that object, unboxed. Exact, not
        // "is a descendant of": a downcast arm selects the arm's class itself,
        // so a descendant of it does not match.
        HirPattern::ClassDowncast {
            class_name,
            binding,
            binding_ty,
        } => {
            // `Box<i64>` as well as `Shape`: the type arguments name what the
            // interface's methods produce, and a `type_id` compare against
            // word 0 does not read them.
            let (Type::Named(iface) | Type::Generic(iface, _)) = scrutinee_ty else {
                return false;
            };
            if !(ctx.is_interface)(iface)
                || !matches!(binding_ty, Type::Named(n) if n == class_name)
                || !ctx.supported_class(&class_name.to_string())
                || ctx.classes.class_type_id(class_name).is_none()
            {
                return false;
            }
            names.insert(binding.as_str(), Cow::Borrowed(binding_ty));
            true
        }
    }
}

/// The result type of a builtin `Option`/`Result` method the walker emits, or
/// `None` when the receiver/method pair is outside the subset.
///
/// A `void` payload is excluded throughout because the unwrap family would read
/// a payload slot that an all-`void` variant does not allocate, and the
/// callable-taking combinators would build one.
///
/// The combinators (`map`/`map_err`/`and_then`/`or_else`) came with function
/// values in. Each rule below reconstructs the type the SHARED
/// emitter actually builds; the caller then compares it against the checker's
/// own type for the expression, so a disagreement refuses the body rather
/// than being reinterpreted here.
///
/// `args` are the call's argument types, so arity and the default/message
/// operand are vetted here rather than at two call sites.
/// The return type of a `fn(...) -> R` value. Eligibility has already proved
/// every combinator operand is one, so a non-function here is a compiler bug.
pub(super) fn fn_return_type(f_ty: &Type) -> Type {
    match f_ty {
        Type::Fn(_, ret) => (**ret).clone(),
        _ => unreachable!("combinator operand vetted by eligibility is a function"),
    }
}

/// The interpolated operands of a `format(spec,..)` call, or `None` when the
/// call does not have the shape the emitter assumes.
///
/// The spec is re-parsed here rather than trusted, and each operand is checked
/// against its own placeholder, for one reason: the emitter walks SEGMENTS and
/// pulls an operand per placeholder, so a spec and an argument list that
/// disagree would silently render the wrong argument — or render a GC pointer
/// through `willow_i64_to_string`. The checker enforces the same three rules
/// (E1401/E0201), so this rejects only synthesized nodes and keeps the walker
/// from having to trust them.
#[cfg(test)]
pub(super) fn format_operands(args: &[HirExpr]) -> Option<&[HirExpr]> {
    let HirExprKind::Str(spec) = &args.first()?.kind else {
        return None;
    };
    let segments = crate::interpolate::parse_spec(spec).ok()?;
    let placeholders: Vec<_> = segments
        .iter()
        .filter(|s| !matches!(s, crate::interpolate::Segment::Literal(_)))
        .collect();
    let operands = &args[1..];
    if placeholders.len() != operands.len() {
        return None;
    }
    let renderable = placeholders.iter().zip(operands).all(|(seg, a)| match seg {
        // A precision placeholder passes its operand straight to an f64
        // formatting symbol, so nothing else may reach it.
        crate::interpolate::Segment::F64(_) => a.ty == Type::F64,
        _ => matches!(a.ty, Type::I64 | Type::F64 | Type::Bool | Type::String),
    });
    renderable.then_some(operands)
}

/// Whether `e` is a `panic(...)` the walker may emit, with a message shape it
/// can assemble.
///
/// Divergence is a property of the POSITION, not of the expression: the emitter
/// ends the current Cranelift block with a `trap`, so a `panic` nested inside
/// an operand would strand the instructions that consume its value after a
/// terminator. Callers therefore ask this only where nothing else follows in
/// the same block — a whole statement, or a whole `match` arm.
#[willow_continuations::lir_validation]
#[cfg(test)]
pub(super) fn supported_panic<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> bool {
    let HirExprKind::Call { callee, args } = &e.kind else {
        return false;
    };
    // A local binding of the same name is an ordinary indirect call, not the
    // builtin, and it wins here exactly as it does in `supported_expr`.
    if !callee.is_free_named("panic")
        || e.ty != Type::Never
        || names.contains_key(callee.unqualified_name().as_deref().unwrap_or(""))
    {
        return false;
    }
    // The three accepted message shapes: none (emission substitutes a
    // default literal), one `String`, or a literal spec plus its operands.
    match args.len() {
        0 => true,
        1 => args[0].ty == Type::String && supported_expr(&args[0], ctx, names),
        _ => format_operands(args)
            .is_some_and(|operands| operands.iter().all(|a| supported_expr(a, ctx, names))),
    }
}

/// The result type of a scalar `toString`, or `None` when this receiver and
/// method are not one — the walker's only entry point into the intrinsic table.
///
/// The four scalar conversions are the whole set with a primitive receiver, and
/// they are matched by INTRINSIC rather than by name so that a future builtin
/// on `i64` does not silently inherit this lowering.
#[cfg(test)]
pub(super) fn scalar_to_string(recv: &Type, method: &str, args: &[HirExpr]) -> Option<Type> {
    let resolved = intrinsics::resolve(recv, method, args.len())?;
    matches!(
        resolved.intrinsic,
        Intrinsic::I64ToString
            | Intrinsic::F64ToString
            | Intrinsic::BoolToString
            | Intrinsic::StringToString
    )
    .then(|| resolved.return_type(|i| args.get(i).map(|a| a.ty.clone())))
}

/// The result type of a `Task<T>`/`JoinHandle<T>` cancellation method, or
/// `None` when this receiver and method are not one.
///
/// Matched by INTRINSIC rather than by name, for the same reason
/// [`scalar_to_string`] is: a future `Task` builtin spelled `cancel` must not
/// silently inherit this lowering.
#[cfg(test)]
fn task_handle_method(recv: &Type, method: &str, arity: usize) -> Option<Type> {
    let resolved = intrinsics::resolve(recv, method, arity)?;
    matches!(
        resolved.intrinsic,
        Intrinsic::TaskCancel | Intrinsic::TaskIsCancelled | Intrinsic::TaskResult
    )
    .then(|| resolved.return_type(|_| None))
}

/// One of the two atomic cells, and the width every operand
/// and result of its methods has.
///
/// `AtomicI64` and `AtomicBool` share one runtime shape — a GC-allocated word
/// the runtime reads and writes atomically — and differ only in that width and
/// in the arithmetic `AtomicBool` does not have. Carrying the distinction as a
/// value keeps the emitter from re-deriving it from the class spelling.
#[derive(Clone, Copy)]
pub(super) struct AtomicCell {
    pub(super) is_i64: bool,
}

impl AtomicCell {
    /// The type of the cell's word: the type of every operand it takes and of
    /// every non-`void` result it produces.
    pub(super) fn word(self) -> Type {
        if self.is_i64 { Type::I64 } else { Type::Bool }
    }

    /// The runtime symbol suffix — `willow_atomic_{suffix}_{op}`.
    pub(super) fn suffix(self) -> &'static str {
        if self.is_i64 { "i64" } else { "bool" }
    }

    /// The prelude name this cell is constructed through.
    pub(super) fn class_name(self) -> &'static str {
        if self.is_i64 {
            "AtomicI64"
        } else {
            "AtomicBool"
        }
    }
}

/// Whether `ty` is an atomic cell, resolved through [`builtin_types`] rather
/// than by name: those two names are owned by the prelude, so a user class
/// cannot take them, and going through the resolver is what makes that true
/// here as well as in `intrinsics::resolve`.
pub(super) fn atomic_cell(ty: &Type) -> Option<AtomicCell> {
    let resolved = builtin_types::resolve(ty)?;
    if !resolved.args.is_empty() {
        return None;
    }
    match resolved.id {
        B::AtomicI64 => Some(AtomicCell { is_i64: true }),
        B::AtomicBool => Some(AtomicCell { is_i64: false }),
        _ => None,
    }
}

/// The intrinsic and result type of an atomic cell's method, or `None` when
/// this receiver and method are not one.
///
/// Matched by INTRINSIC for the same reason [`task_handle_method`] is, and that
/// is also what keeps `AtomicBool::add` out of the walker: the resolver never
/// produces an arithmetic intrinsic for a bool cell, so the emitter never has
/// to know which operations each width implements.
#[cfg(test)]
fn atomic_method(recv: &Type, method: &str, arity: usize) -> Option<(Intrinsic, Type)> {
    let resolved = intrinsics::resolve(recv, method, arity)?;
    matches!(
        resolved.intrinsic,
        Intrinsic::AtomicLoad
            | Intrinsic::AtomicStore
            | Intrinsic::AtomicCompareExchange
            | Intrinsic::AtomicSwap
            | Intrinsic::AtomicAdd
            | Intrinsic::AtomicSub
    )
    .then(|| (resolved.intrinsic, resolved.return_type(|_| None)))
}

/// One of the two cancellation handles: a `CancellationToken`
/// or a `TaskScope`.
///
/// The two expose the same four operations over different runtime objects, so
/// what differs is the runtime symbol PREFIX rather than the operation. A scope
/// additionally has `finish`.
#[derive(Clone, Copy)]
pub(super) struct CancelHandle {
    is_token: bool,
}

impl CancelHandle {
    /// The runtime symbol prefix — `{prefix}_{op}`.
    pub(super) fn prefix(self) -> &'static str {
        if self.is_token {
            "willow_cancellation_token"
        } else {
            "willow_task_scope"
        }
    }

    /// The prelude name this handle is constructed through.
    pub(super) fn class_name(self) -> &'static str {
        if self.is_token {
            "CancellationToken"
        } else {
            "TaskScope"
        }
    }
}

/// Whether `ty` is a cancellation handle, resolved through [`builtin_types`]
/// rather than by name, for the same reason [`atomic_cell`] is: the prelude owns
/// both names, and going through the resolver is what makes that true here as
/// well as in `intrinsics::resolve`.
pub(super) fn cancellation_handle(ty: &Type) -> Option<CancelHandle> {
    let resolved = builtin_types::resolve(ty)?;
    if !resolved.args.is_empty() {
        return None;
    }
    match resolved.id {
        B::CancellationToken => Some(CancelHandle { is_token: true }),
        B::TaskScope => Some(CancelHandle { is_token: false }),
        _ => None,
    }
}

/// One of the two native-blocking cells: a `BlockingCell<T>`
/// or a `BlockingRwCell<T>`.
///
/// Unlike `Mutex<T>` and `RwLock<T>`, these two are single-operation cells with
/// no critical section: each accessor is one runtime call that blocks the OS
/// thread on contention rather than parking the task, which is why they are
/// callable from a synchronous function. What differs between them is the
/// runtime symbol PREFIX and the method names, and the method names come from
/// the resolver rather than from here.
#[derive(Clone, Copy)]
pub(super) struct BlockingCellKind {
    is_rw: bool,
}

impl BlockingCellKind {
    /// The runtime symbol prefix — `{prefix}_{op}`.
    pub(super) fn prefix(self) -> &'static str {
        if self.is_rw {
            "willow_blocking_rw_cell"
        } else {
            "willow_blocking_cell"
        }
    }

    /// The prelude name this cell is constructed through.
    pub(super) fn class_name(self) -> &'static str {
        if self.is_rw {
            "BlockingRwCell"
        } else {
            "BlockingCell"
        }
    }
}

/// Whether `ty` is a blocking cell, and over which element type — resolved
/// through [`builtin_types`] rather than by name, for the same reason
/// [`atomic_cell`] is: the prelude owns both names.
///
/// The element type is returned because the cell's ABI is word-based: the
/// emitter has to coerce INTO a word on the way in and back OUT of one on the
/// way out, and only the element type says how.
pub(super) fn blocking_cell(ty: &Type) -> Option<(BlockingCellKind, &Type)> {
    let resolved = builtin_types::resolve(ty)?;
    let [elem] = resolved.args else {
        return None;
    };
    match resolved.id {
        B::BlockingCell => Some((BlockingCellKind { is_rw: false }, elem)),
        B::BlockingRwCell => Some((BlockingCellKind { is_rw: true }, elem)),
        _ => None,
    }
}

/// Whether `ty` is a scheduler-aware lock, and over which protected type
///.
///
/// Matched by NAME rather than through [`builtin_types`], because these two are
/// not builtin type ids: the checker keys `lock`/`read`/`write` on exactly these
/// two spellings too ([`crate::parser::ast::LockMode::lock_type_name`]).
///
/// The `&'static str` is the runtime symbol PREFIX, which is what distinguishes
/// the two state machines: `Mutex<T>` has one owner, `RwLock<T>` has readers and
/// a writer, and their acquire/poll/load/release entry points are separate.
pub(super) fn scheduler_lock(ty: &Type) -> Option<(&'static str, &Type)> {
    let Type::Generic(name, args) = ty else {
        return None;
    };
    let [protected] = args.as_slice() else {
        return None;
    };
    match name.name().as_ref() {
        "Mutex" => Some(("willow_async_mutex", protected)),
        "RwLock" => Some(("willow_async_rwlock", protected)),
        _ => None,
    }
}

/// The intrinsic and result type of a blocking cell's method, or `None` when
/// this receiver and method are not one.
///
/// Matched by INTRINSIC, like [`atomic_method`], and that is what keeps the two
/// cells' method names apart without the walker knowing them: the resolver
/// never answers `get`/`set` for a `BlockingRwCell` or `read`/`write` for a
/// `BlockingCell`.
#[cfg(test)]
fn blocking_cell_method(recv: &Type, method: &str, arity: usize) -> Option<(Intrinsic, Type)> {
    let resolved = intrinsics::resolve(recv, method, arity)?;
    matches!(
        resolved.intrinsic,
        Intrinsic::CellGet | Intrinsic::CellSet | Intrinsic::RwCellRead | Intrinsic::RwCellWrite
    )
    .then(|| (resolved.intrinsic, resolved.return_type(|_| None)))
}

/// The intrinsic and result type of a cancellation handle's method, or `None`
/// when this receiver and method are not one.
///
/// Matched by INTRINSIC, like [`atomic_method`], and that is also what keeps
/// `CancellationToken::finish` out of the walker for free: the resolver never
/// produces `ScopeFinish` for a token.
///
/// `attach` and `add` hand back the very task they were given, so their result
/// type is their argument's — `args` is what supplies it. Passing a closure that
/// answers `None` would silently type them `Task<void>`.
#[cfg(test)]
fn cancellation_method(recv: &Type, method: &str, args: &[HirExpr]) -> Option<(Intrinsic, Type)> {
    let resolved = intrinsics::resolve(recv, method, args.len())?;
    matches!(
        resolved.intrinsic,
        Intrinsic::TokenIsCancelled
            | Intrinsic::TokenCancel
            | Intrinsic::TokenChild
            | Intrinsic::TokenAttach
            | Intrinsic::ScopeIsCancelled
            | Intrinsic::ScopeCancel
            | Intrinsic::ScopeChild
            | Intrinsic::ScopeAdd
            | Intrinsic::ScopeFinish
    )
    .then(|| {
        let ret = resolved.return_type(|i| args.get(i).map(|a: &HirExpr| a.ty.clone()));
        (resolved.intrinsic, ret)
    })
}

pub(super) fn option_result_method(recv: &Type, method: &str, args: &[Type]) -> Option<Type> {
    let resolved = builtin_types::resolve(recv)?;
    let payload = |i: usize| -> Option<Type> {
        resolved
            .args
            .get(i)
            .filter(|t| !matches!(t, Type::Void))
            .cloned()
    };
    let no_args = |t: Type| args.is_empty().then_some(t);
    // `expect(msg)` takes exactly one `String`; `unwrap_or(default)` takes
    // exactly one value of the payload type. Emission passes both
    // straight through with no coercion, so `assignable_repr` is the rule.
    let one_string = |t: Type| matches!(args, [Type::String]).then_some(t);
    let one_of = |t: Type| matches!(args, [a] if assignable_repr(&t, a)).then_some(t);
    // The single callable operand, given the parameter list the emitter feeds
    // it. Yields the callable's return type, which is what every combinator's
    // result is built from.
    let one_fn = |params: &[Type]| -> Option<Type> {
        match args {
            [Type::Fn(ps, ret)] if ps.as_slice() == params => Some((**ret).clone()),
            _ => None,
        }
    };
    let non_void = |t: Type| (!matches!(t, Type::Void)).then_some(t);
    let option_of = |t: Type| Type::Generic("Option".to_string().into(), vec![t]);
    let result_of = |ok: Type, err: Type| Type::Generic("Result".to_string().into(), vec![ok, err]);
    match resolved.id {
        B::Option => match method {
            "is_some" | "is_none" => no_args(Type::Bool),
            "unwrap" => no_args(payload(0)?),
            "expect" => one_string(payload(0)?),
            "unwrap_or" => one_of(payload(0)?),
            // `Option<T>::map(fn(T) -> U) -> Option<U>`.
            "map" => Some(option_of(non_void(one_fn(&[payload(0)?])?)?)),
            // `Option<T>::and_then(fn(T) -> Option<U>) -> Option<U>`: the
            // emitter reads `U` out of the callable's return type to build the
            // `None` arm, so that return type IS the result.
            "and_then" => {
                let produced = one_fn(&[payload(0)?])?;
                builtin_types::unary_arg(&produced, B::Option)
                    .filter(|u| !matches!(u, Type::Void))?;
                Some(produced)
            }
            // `Option<T>::or_else(fn -> Option<T>) -> Option<T>`: the `Some`
            // arm passes the RECEIVER through, so the callable must produce the
            // receiver's payload type — that payload is what picks the pointer
            // niche over the box, so both arms merge one representation.
            "or_else" => {
                let produced = one_fn(&[])?;
                (builtin_types::unary_arg(&produced, B::Option) == Some(&payload(0)?))
                    .then_some(produced)
            }
            _ => None,
        },
        B::Result => match method {
            "is_ok" | "is_err" => no_args(Type::Bool),
            "unwrap" => no_args(payload(0)?),
            "unwrap_err" => no_args(payload(1)?),
            "expect" => one_string(payload(0)?),
            "unwrap_or" => one_of(payload(0)?),
            // `Result<T, E>::map(fn(T) -> U) -> Result<U, E>`; the `Err` arm
            // passes the receiver through, so `E` is unchanged.
            "map" => Some(result_of(non_void(one_fn(&[payload(0)?])?)?, payload(1)?)),
            // `Result<T, E>::map_err(fn(E) -> F) -> Result<T, F>`.
            "map_err" => Some(result_of(payload(0)?, non_void(one_fn(&[payload(1)?])?)?)),
            // `Result<T, E>::and_then(fn(T) -> Result<U, E>) -> Result<U, E>`.
            // The result is the CALLABLE's return type, which is also what the
            // checker records: the `Err` arm passes the receiver through, and
            // that is representation-safe because every `Result` is the same
            // two-word box whatever its type arguments.
            //
            // NEITHER error type is constrained, the receiver's included. A
            // `Result::Ok(5)` written with nothing to infer `E` from records
            // `Result<i64, void>`, and requiring a non-void one here took the
            // whole enclosing function out of LIR for a receiver
            // whose error type `emit_result_chain_side` reads only to
            // rewrap a pass-through `Err` whose representation changes.
            "and_then" => {
                let produced = one_fn(&[payload(0)?])?;
                let (ok, _) = builtin_types::binary_args(&produced, B::Result)?;
                non_void(ok.clone()).map(|_| produced)
            }
            // `Result<T, E>::or_else(fn(E) -> Result<T, F>) -> Result<T, F>`:
            // the mirror image — the `Ok` arm passes the receiver through, so
            // only the ok payload has to line up.
            "or_else" => {
                let produced = one_fn(&[payload(1)?])?;
                let (ok, _) = builtin_types::binary_args(&produced, B::Result)?;
                (*ok == payload(0)?).then_some(produced)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Where a `match` sits, which decides whether it may be one that leaves.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(super) enum MatchPosition {
    /// An operand: something after the `match` reads the value it hands to its
    /// merge block, so some path through it has to reach that block.
    Operand,
    /// A whole statement, or the tail of a body — nothing follows it in the
    /// same Cranelift block, so a `match` every arm of which leaves is fine.
    Divergent,
}

/// Whether every path through a `match` with these arms leaves, making its
/// merge block unreachable.
///
/// The checker's own type for the `match` is not the test. It types one `!`
/// where the surrounding code needs that, but the same all-arms-leave `match`
/// written as a function body's ending is typed by its arms instead, and the
/// walker used to refuse that shape.
#[cfg(test)]
pub(super) fn match_diverges(arms: &[HirMatchArm]) -> bool {
    !arms.is_empty() && arms.iter().all(arm_diverges)
}

/// The `match` rule, shared by the value form (through [`supported_expr`]) and
/// the diverging form (through [`supported_divergent_expr`]).
///
/// `e` is the match expression itself; its type is what every arm that reaches
/// the merge block must produce.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_match<'n>(
    e: &'n HirExpr,
    scrutinee: &'n HirExpr,
    arms: &'n [HirMatchArm],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    position: MatchPosition,
) -> bool {
    // An arm-less match has no value to produce and the checker should
    // have rejected it; refusing here keeps the emitter's "seed the
    // result variable, then every arm overwrites it" invariant honest.
    if arms.is_empty() || !supported_expr(scrutinee, ctx, names) {
        return false;
    }
    // The scrutinee shapes the walker can test. A `String` or class scrutinee
    // needs a content comparison the pattern emitter below does not have.
    let scrutinee_ok = match &scrutinee.ty {
        Type::I64 | Type::Bool => true,
        // An interface box, matched by class downcast patterns
        //. Checked before the enum test because an interface
        // is also spelled `Type::Named`; the two sets cannot overlap. A GENERIC
        // interface instantiation — `Box<i64>` — is one too:
        // the box has the same two words whatever the type arguments say, and
        // the downcast reads word 0's `type_id` without consulting them.
        Type::Named(n) | Type::Generic(n, _) if (ctx.is_interface)(n) => true,
        // Any enum instance, generic or not: `Color`, `Option<i64>`,
        // `Result<i64, String>`.
        Type::Named(_) | Type::Generic(..) => ctx.supported_enum_type(&scrutinee.ty),
        _ => false,
    };
    if !scrutinee_ok {
        return false;
    }
    // A match that produces a value needs at least one arm that reaches the
    // merge block, where the value is read. One all of whose arms leave hands
    // nothing to anything, so it is admissible only in the positions
    // [`supported_divergent_expr`] is asked about.
    if position == MatchPosition::Operand && match_diverges(arms) {
        return false;
    }
    arms.iter().all(|arm| {
        let mut arm_names = names.clone();
        if !supported_pattern(&arm.pattern, &scrutinee.ty, ctx, &mut arm_names) {
            return false;
        }
        if let Some(guard) = &arm.guard
            && !supported_expr(guard, ctx, &arm_names)
        {
            return false;
        }
        if arm_diverges(arm) {
            // A diverging arm hands nothing to the merge block, so the
            // representation agreement below is not asked of it.
            return supported_divergent_body(&arm.body, ctx, &arm_names, BodyScope::Bracketed);
        }
        if e.ty == Type::Void {
            return supported_effect_body(&arm.body, ctx, &arm_names, BodyScope::Bracketed);
        }
        // An arm that produces a value is a single expression. Not a walker
        // limit — the grammar's: a block-bodied arm's last statement needs a
        // `;` (E0101), so such an arm is typed `void` and is checked above.
        let [HirStmt::Expr(value)] = arm.body.as_slice() else {
            return false;
        };
        ctx.same_repr(&e.ty, &value.ty) && supported_expr(value, ctx, &arm_names)
    })
}

/// Which binding forms a body may contain. This is a property of the EMITTER
/// that will run the body, not of the statements themselves.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(super) enum BodyScope {
    /// A `match` arm or a `select` case. The emitter snapshots `vars` and the
    /// GC root depth before the body and restores them after, so a `let` is
    /// admitted: its rooted slot is popped when the body ends, on the ordinary
    /// path by the bracket and on a `return` by [`FuncGen::emit_lir_return`]'s
    /// full-depth pop.
    Bracketed,
}

/// One statement of a body the walker runs for its effect.
///
/// Assignment and `let` join plain expressions here because a `match` used as a
/// statement is how the source spells a multi-way store — the `defer match
/// recover {... }` of `example/panic_recover_service.wi` is one
///. On success a `let`'s name is added to `names`, so the
/// rest of the body is checked against the scope the emitter will actually run.
///
/// `if` is here for a different reason: LIR lowers a function-level `if` into
/// blocks of the graph, but these bodies stay in HIR shape, so the branches of
/// an `if` inside one have nowhere else to be decided.
///
/// Suspension is refused for both binding forms. An arm body has no cooperative
/// await split of its own, so an `await` in an initialiser would reach the
/// emitter unhoisted; and a binding held in a Cranelift variable does not
/// survive a park. The enclosing statement's own check
/// ([`lir_async_rejection_reason`]) already rejects a suspending `match` arm,
/// but it does not look inside a `defer` body, so the rule is stated here where
/// both are covered.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_body_stmt<'n>(
    stmt: &'n HirStmt,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    match stmt {
        HirStmt::Expr(e) => supported_expr(e, ctx, names),
        // The DECLARED type of the target decides the store, exactly as it does
        // for `LirInst::Assign`, so `a = new Dog;` on an `Animal` local boxes.
        // A name that is not in scope here is one the walker cannot resolve — a
        // bare enum variant, a function used as a value — not a missing binding.
        HirStmt::Assign { name, value, .. } => {
            let Some(declared) = names.get(name.as_str()).cloned() else {
                return false;
            };
            !lir_expr_suspends(value)
                && ctx.storable(&declared, &value.ty)
                && supported_expr(value, ctx, names)
        }
        // The same checked heap store as a function-level
        // `LirInst::FieldAssign`. Bracketed HIR islands can mutate an object
        // through the same LIR emission path as ordinary statements
        //. Deferred bodies use the same checked heap store.
        HirStmt::IndexAssign {
            array,
            index,
            value,
            ..
        } => {
            let Type::Array(elem) = &array.ty else {
                return false;
            };
            ctx.storable(elem, &value.ty)
                && index.ty == Type::I64
                && [array, index, value]
                    .iter()
                    .all(|e| !lir_expr_suspends(e) && supported_expr(e, ctx, names))
        }
        HirStmt::FieldAssign {
            object,
            field,
            value,
            ..
        } => {
            let Some(field_ty) = ctx.class_layout_of(&object.ty).and_then(|layout| {
                layout
                    .iter()
                    .find(|(name, _)| name == field)
                    .map(|(_, ty)| ty.clone())
            }) else {
                return false;
            };
            !lir_expr_suspends(object)
                && !lir_expr_suspends(value)
                && ctx.storable(&field_ty, &value.ty)
                && supported_expr(object, ctx, names)
                && supported_expr(value, ctx, names)
        }
        // `Class::prop = value;` inside a body the walker keeps as an HIR
        // island — a `defer` block, a `match` arm, a `select` case
        //. Vetted exactly as the function-level
        // `LirInst::StaticFieldAssign` is, and admitted in EITHER scope: the
        // store's destination is a data segment, so unlike a `let` it needs no
        // storage of its own, which is the only thing a deferred body cannot
        // give a statement.
        HirStmt::StaticFieldAssign {
            class,
            field,
            value,
            ..
        } => {
            // `Self::prop` resolves against the enclosing class, as
            // `emit_lir_static_field_assign` resolves it.
            let Some(field_ty) = (ctx.static_field)(ctx.resolved_class(&class.to_string()), field)
            else {
                return false;
            };
            !lir_expr_suspends(value)
                && ctx.supported_type(&field_ty)
                && ctx.storable(&field_ty, &value.ty)
                && supported_expr(value, ctx, names)
        }
        HirStmt::Let {
            name, ty, value, ..
        } => {
            if lir_expr_suspends(value)
                || !ctx.supported_type(ty)
                || !ctx.storable(ty, &value.ty)
                || !supported_expr(value, ctx, names)
            {
                return false;
            }
            // Shadowing is not a concern: HIR lowering renames a binding that
            // shadows another in the same function (`LowerCtx::bind`), so this
            // name is unique function-wide and cannot displace an outer one.
            names.insert(name.as_str(), Cow::Borrowed(ty));
            true
        }
        // A body kept as an HIR island gets its branches here rather than from
        // the LIR block graph, which only covers function-level control flow
        //. A binding made inside a branch is scoped to that
        // branch, so `names` is not extended from it.
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            cond.ty == Type::Bool
                && !lir_expr_suspends(cond)
                && supported_expr(cond, ctx, names)
                && supported_branch_body(then_branch, ctx, names, scope)
                && else_branch
                    .as_deref()
                    .is_none_or(|body| supported_branch_body(body, ctx, names, scope))
        }
        // A `while` inside an island body, for the same reason an `if` is here:
        // the LIR block graph only covers function-level control flow, so this
        // loop has nowhere else to be decided. Its body is
        // checked as a plain run of effect statements — a `break`, a `continue`
        // or a `return` out of it is not admitted, which is what lets the
        // emitter give the loop two edges and no loop context.
        HirStmt::While { cond, body, .. } => {
            cond.ty == Type::Bool
                && !lir_expr_suspends(cond)
                && supported_expr(cond, ctx, names)
                && supported_effect_body(body, ctx, names, scope)
        }
        // A `for` in an island is desugared by its emitter just like the main
        // HIR -> LIR lowering desugars a function-level loop: the iterable is
        // evaluated once, then a range advances its value or an array advances
        // an index. `break`/`continue`/`return` remain excluded by the body's
        // effect-only check, so no island loop context is required.
        HirStmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            let element = match &iterable.ty {
                Type::Array(element) => Some(element.as_ref()),
                ty if builtin_types::unary_arg(ty, B::FrozenArray).is_some() => {
                    builtin_types::unary_arg(ty, B::FrozenArray)
                }
                ty if range_i64(ty) => Some(&Type::I64),
                _ => None,
            };
            let Some(element) = element else {
                return false;
            };
            if lir_expr_suspends(iterable) || !supported_expr(iterable, ctx, names) {
                return false;
            }
            let mut body_names = names.clone();
            body_names.insert(name.as_str(), Cow::Borrowed(element));
            supported_effect_body(body, ctx, &body_names, scope)
        }
        _ => false,
    }
}

/// One branch of an `if` inside a body: a run of effect statements that either
/// falls through to whatever follows the `if` or leaves the function.
///
/// The emitter gives each branch a Cranelift block of its own and jumps to the
/// join only from the branches that did not terminate, so the two endings need
/// no agreement with each other.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_branch_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    if supported_effect_body(body, ctx, names, scope) {
        return true;
    }
    supported_divergent_body(body, ctx, names, scope)
}

/// Whether the walker can emit `body` for its EFFECT: nothing reads a value
/// from it. A divergent expression ends emission, so statements after it do
/// not need an emission path (including dead statements in a deferred body).
///
/// The map is cloned rather than borrowed mutably because a `let` here is
/// scoped to this body; the caller's own scope must not gain the name.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_effect_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    let mut names = names.clone();
    for stmt in body {
        if let HirStmt::Expr(expr) = stmt
            && supported_divergent_expr(expr, ctx, &names)
        {
            return true;
        }
        if !supported_body_stmt(stmt, ctx, &mut names, scope) {
            return false;
        }
    }
    true
}

#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_select<'n>(
    cases: &'n [HirSelectCase],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    !cases.is_empty()
        && cases.iter().all(|case| {
            let mut case_names = names.clone();
            let kind_ok = match &case.kind {
                HirSelectCaseKind::Recv {
                    binding,
                    channel,
                    closed_aware,
                } => {
                    let Some(elem) = channel_element_type_ref(&channel.ty) else {
                        return false;
                    };
                    if binding != "_" {
                        let bound = if *closed_aware {
                            Cow::Owned(B::Option.apply(vec![elem.clone()]))
                        } else {
                            Cow::Borrowed(elem)
                        };
                        case_names.insert(binding.as_str(), bound);
                    }
                    supported_expr(channel, ctx, names)
                }
                HirSelectCaseKind::Send { channel, value } => channel_element_type_ref(&channel.ty)
                    .is_some_and(|elem| {
                        ctx.storable(elem, &value.ty)
                            && supported_expr(channel, ctx, names)
                            && supported_expr(value, ctx, names)
                    }),
                HirSelectCaseKind::Timeout { millis } => {
                    millis.ty == Type::I64 && supported_expr(millis, ctx, names)
                }
                HirSelectCaseKind::Join { binding, task } => {
                    let Some(bound) = await_output_type(&task.ty) else {
                        return false;
                    };
                    // `await t.result` binds `Result<T, Cancelled>`, a type no
                    // HIR node in this case holds, so the map owns its values.
                    if !ctx.supported_type(&bound) {
                        return false;
                    }
                    if binding != "_" && bound != Type::Void {
                        case_names.insert(binding.as_str(), Cow::Owned(bound));
                    }
                    supported_expr(task, ctx, names)
                }
                HirSelectCaseKind::Default => true,
            };
            // A case body may also LEAVE -- `... => { return "sent"; }` -- which
            // is what [`supported_branch_body`] adds over the plain effect body
            //. The select emitter already brackets each case and
            // only jumps to its merge from the ones that fall through, so a
            // returning case needs nothing more from it.
            kind_ok && supported_branch_body(&case.body, ctx, &case_names, BodyScope::Bracketed)
        })
}

#[cfg(test)]
fn channel_element_type_ref(ty: &Type) -> Option<&Type> {
    builtin_types::unary_arg(ty, B::Channel)
}

/// This arm ends by leaving the function or by unwinding, so it hands no value
/// to the merge block.
#[cfg(test)]
pub(super) fn arm_diverges(arm: &HirMatchArm) -> bool {
    body_diverges(&arm.body)
}

/// Whether every path through `body` leaves.
///
/// A `return` is one syntactically and `!` is the type of every tail expression
/// that unwinds, but neither test sees an `if` both of whose branches leave —
/// the `if` itself is typed nothing, and the statement after it is unreachable
/// rather than absent. Reading that shape is what lets an arm guard a path with
/// an early `return` and still count as leaving.
#[cfg(test)]
pub(super) fn body_diverges(body: &[HirStmt]) -> bool {
    let mut pending = vec![body];
    while let Some(body) = pending.pop() {
        match body.last() {
            Some(HirStmt::Return { .. }) => {}
            Some(HirStmt::Expr(e)) if e.ty == Type::Never => {}
            Some(HirStmt::Expr(HirExpr {
                kind: HirExprKind::Match { arms, .. },
                ..
            })) if !arms.is_empty() => {
                pending.extend(arms.iter().rev().map(|arm| arm.body.as_slice()));
            }
            Some(HirStmt::If {
                then_branch,
                else_branch: Some(else_branch),
                ..
            }) => {
                pending.push(else_branch);
                pending.push(then_branch);
            }
            _ => return false,
        }
    }
    true
}

/// Whether the walker can emit `body` where nothing follows it in the same
/// Cranelift block: a run of ordinary effect statements ending in something
/// that leaves — `return`, `panic(...)`, or a `match` all of whose arms do
///.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_divergent_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    let Some((last, leading)) = body.split_last() else {
        return false;
    };
    // Only effect statements may precede the tail. One of them may still end a
    // path — an `if` whose branch returns — but never every path: `supported_expr`
    // refuses a `!`-typed expression, so no leading statement diverges outright.
    let mut names = names.clone();
    let names = &mut names;
    let leading_ok = leading
        .iter()
        .all(|s| supported_body_stmt(s, ctx, names, scope));
    let last_ok = match last {
        HirStmt::Return { value: None, .. } => true,
        HirStmt::Return {
            value: Some(value), ..
        } => ctx.storable(ctx.return_type, &value.ty) && supported_expr(value, ctx, names),
        HirStmt::Expr(e) => supported_divergent_expr(e, ctx, names),
        // An `if` both of whose branches leave. Its own rule lives in
        // [`supported_body_stmt`]; what this arm adds is that such an `if` may
        // be a body's ending, with nothing after it to fall through to.
        HirStmt::If { .. } if body_diverges(std::slice::from_ref(last)) => {
            supported_body_stmt(last, ctx, names, scope)
        }
        _ => false,
    };
    leading_ok && last_ok
}

/// An expression the walker emits for its EFFECT in a position where nothing
/// follows it in the same Cranelift block — a whole statement, or the tail of a
/// match arm. Both forms end the block, which is why the position matters:
/// nested in an operand, either would strand the instructions that consume its
/// value after a terminator.
#[willow_continuations::lir_validation]
#[cfg(test)]
pub(super) fn supported_divergent_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    if supported_panic(e, ctx, names) {
        return true;
    }
    match &e.kind {
        HirExprKind::Match { scrutinee, arms } if e.ty == Type::Never || match_diverges(arms) => {
            supported_match(e, scrutinee, arms, ctx, names, MatchPosition::Divergent)
        }
        _ => false,
    }
}

/// `'n` ties the borrowed names to the expression tree being vetted, so a
/// `match` arm can extend the map with its own pattern bindings — which live in
/// that same tree — before checking the arm body.
#[willow_continuations::lir_validation]
#[cfg(test)]
pub(super) fn supported_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    validate_expr(e, ctx, names, false).0
}

/// Cache results by node address for this borrowed tree and this lexical scope.
/// A rule may inspect a grandchild (await constructors and reference places),
/// or deliberately ignore a structural child (namespace receivers). Evaluating
/// the original rules against cached results preserves both cases; blindly
/// requiring every structural child to pass would change the accepted subset.
#[willow_continuations::lir_validation]
#[cfg(test)]
fn validate_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    diagnose: bool,
) -> (bool, Option<&'n HirExpr>) {
    let mut pending = vec![(e, false)];
    let mut results: HashMap<*const HirExpr, (bool, Option<&HirExpr>)> = HashMap::new();
    while let Some((expr, visited)) = pending.pop() {
        let scoped = matches!(
            expr.kind,
            HirExprKind::Lambda { .. } | HirExprKind::Match { .. } | HirExprKind::Select { .. }
        );
        if !visited && !scoped {
            pending.push((expr, true));
            pending.extend(
                expr.children()
                    .into_iter()
                    .rev()
                    .map(|child| (child, false)),
            );
            continue;
        }
        let accepted = supported_expr_node(expr, ctx, names, &|child| {
            results[&(child as *const HirExpr)].0
        });
        let rejection = if !diagnose || accepted {
            None
        } else if scoped {
            minimal_unsupported_scoped_expr(expr, ctx, names)
        } else {
            expr.children()
                .into_iter()
                .find_map(|child| results[&(child as *const HirExpr)].1)
                .or(Some(expr))
        };
        results.insert(expr, (accepted, rejection));
    }
    results[&(e as *const HirExpr)]
}

#[willow_continuations::lir_validation]
#[cfg(test)]
fn supported_expr_node<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    child_supported: &impl Fn(&'n HirExpr) -> bool,
) -> bool {
    if !ctx.supported_type(&e.ty) && !is_fresh_empty_map(e) {
        return false;
    }
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => true,
        HirExprKind::Str(_) => true,
        HirExprKind::Var(name) => names
            .get(name.as_str())
            .is_some_and(|bound| ctx.same_repr(bound, &e.ty)),
        HirExprKind::ReferenceArg { place } => {
            ctx.same_repr(&place.ty, &e.ty)
                && supported_reference_place(place, ctx, names, child_supported)
        }
        // A named function used as a value. The declared
        // signature must be the type this expression carries: the pointer is
        // later CALLED through a Cranelift signature built from `e.ty`, so a
        // disagreement would call the target under the wrong ABI.
        HirExprKind::FnRef(name) => ctx
            .fn_value_of(name)
            .is_some_and(|ty| ctx.same_callable(&ty, &e.ty)),
        // A lambda evaluates to its lifted function — the address itself for a
        // `fn`, an environment object carrying that address for a `closure`. Its
        // BODY is not this function's problem: that is compiled under its own
        // symbol and vetted on its own terms. What is checked here is the same
        // thing as for a named function — the symbol exists and its declared
        // signature is the type the expression carries — plus, for a closure,
        // that every value the environment is built from is a name THIS
        // function can resolve and a word the walker can copy.
        HirExprKind::Lambda { id, captures, .. } => {
            (ctx.lambda_symbol)(*id)
                .and_then(|sym| ctx.fn_value_of(&sym))
                .is_some_and(|ty| ctx.same_callable(&ty, &e.ty))
                && captures.iter().all(|c| {
                    names
                        .get(c.source.as_str())
                        .is_some_and(|bound| ctx.repr_compatible(&c.ty, bound))
                })
        }
        HirExprKind::Binary { op, lhs, rhs } => {
            // On strings only `+` (concat) and content comparison are emitted.
            if lhs.ty == Type::String && !matches!(op, BinOp::Add | BinOp::Eq | BinOp::Ne) {
                return false;
            }
            if matches!(op, BinOp::Pow)
                && !matches!(
                    (&lhs.ty, &rhs.ty),
                    (Type::I64, Type::I64) | (Type::F64, Type::F64)
                )
            {
                return false;
            }
            // Class values have no operators here: `==` on two objects is an
            // identity comparison that value lowering emits as an LIR `Binary`
            //, never through this walker. A payload-free enum
            // is the exception — the value IS its tag, so `==` and `!=` are the
            // integer comparison below and mean what the program wrote; both
            // sides must be the same enum, which is also what lets an aliased
            // spelling be compared with the canonical one.
            if matches!(lhs.ty, Type::Named(_)) || matches!(rhs.ty, Type::Named(_)) {
                let compares_tags = matches!(op, BinOp::Eq | BinOp::Ne)
                    && ctx.tag_immediate_enum(&lhs.ty)
                    && ctx.tag_immediate_enum(&rhs.ty)
                    && ctx.same_repr(&lhs.ty, &rhs.ty);
                if !compares_tags {
                    return false;
                }
            }
            child_supported(lhs) && child_supported(rhs)
        }
        // Both arms feed one Cranelift variable, and the walker inserts no
        // conversion between them — a `cond ? new Dog: new Cat` typed
        // `Animal` would define that variable with two raw class pointers.
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            ctx.same_repr(&e.ty, &then_expr.ty)
                && ctx.same_repr(&e.ty, &else_expr.ty)
                && child_supported(condition)
                && child_supported(then_expr)
                && child_supported(else_expr)
        }
        // `match` as an expression. Every arm feeds one
        // Cranelift variable, so the same no-conversion rule `Ternary` states
        // applies to each arm body.
        HirExprKind::Match { scrutinee, arms } => {
            supported_match(e, scrutinee, arms, ctx, names, MatchPosition::Operand)
        }
        HirExprKind::Unary { operand, .. } => child_supported(operand),
        HirExprKind::Invoke { callee, args, .. } => {
            let (Type::Fn(params, ret) | Type::Closure(params, ret)) = &callee.ty else {
                return false;
            };
            child_supported(callee)
                && ctx.same_repr(ret, &e.ty)
                && params.len() == args.len()
                && params.iter().zip(args).all(|(ty, arg)| {
                    ctx.supported_type(ty) && ctx.storable(ty, &arg.ty) && child_supported(arg)
                })
        }
        HirExprKind::Call { callee, args } => {
            // HIR spells direct and indirect calls with the same node, and a
            // local fn-typed binding shadows a free function.
            // The local wins here because this must resolve to what the type checker
            // checked the call against.
            if let Some(local) = names.get(callee.unqualified_name().as_deref().unwrap_or("")) {
                // Either callable value: a `closure` is called through the code
                // pointer in its word 0, with the object itself as the hidden
                // leading argument, so the only difference from a `fn` is in
                // the emitter.
                let (Type::Fn(params, ret) | Type::Closure(params, ret)) = local.as_ref() else {
                    return false;
                };
                return ctx.same_repr(ret, &e.ty)
                    && params.len() == args.len()
                    && args
                        .iter()
                        .all(|arg| !matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                    && params
                        .iter()
                        .zip(args)
                        .all(|(p, a)| ctx.supported_type(p) && ctx.storable(p, &a.ty))
                    && args.iter().all(child_supported);
            }
            if matches!(
                callee.unqualified_name().as_deref().unwrap_or(""),
                "sleep" | "yield"
            ) {
                return builtin_types::unary_arg(&e.ty, B::Future) == Some(&Type::Void)
                    && match (
                        callee.unqualified_name().as_deref().unwrap_or(""),
                        args.as_slice(),
                    ) {
                        ("sleep", [millis]) => millis.ty == Type::I64 && child_supported(millis),
                        ("yield", []) => true,
                        _ => false,
                    };
            }
            // `format` is variadic and has no function symbol: it assembles a
            // string from a literal spec at the call site.
            if callee.is_free_named("format") {
                return e.ty == Type::String
                    && format_operands(args)
                        .is_some_and(|operands| operands.iter().all(child_supported));
            }
            // `references.wi` deliberately collects inside reference-taking
            // functions to prove pointed-to GC values stay alive. These builtins
            // are zero-argument runtime calls and need no AST-only metadata.
            // `gc_minor_collect` is the MOVING one, and is what a test has to
            // reach to prove a binding survives evacuation.
            if callee.is_free_named("gc_collect")
                || callee.is_free_named("gc_minor_collect")
                || callee.is_free_named("gc_task_frame_diagnostics")
            {
                return args.is_empty() && e.ty == Type::Void;
            }
            // Their read-only siblings: `gc_allocated_bytes` and the other
            // statistic counters. Same shape — a zero-argument runtime call
            // with no AST-only metadata — but they return the counter
            //.
            if runtime_stat_builtin_runtime_name(callee.unqualified_name().as_deref().unwrap_or(""))
                .is_some()
            {
                return args.is_empty() && e.ty == Type::I64;
            }
            if callee.is_free_named("recover") {
                return args.is_empty()
                    && builtin_types::unary_arg(&e.ty, B::Option).is_some_and(|payload| {
                        payload == &Type::Named("PanicInfo".to_string().into())
                    });
            }
            // These compiler-known control-flow operations require lexical
            // panic-scope handling through the dedicated paths above.
            !matches!(
                callee.unqualified_name().as_deref().unwrap_or(""),
                "panic" | "recover"
            ) && ctx.callable(
                callee.unqualified_name().as_deref().unwrap_or(""),
                args,
                false,
            ) && args.iter().all(child_supported)
        }
        HirExprKind::Print { value, newline: _ } => {
            (scalar(&value.ty) || value.ty == Type::String) && child_supported(value)
        }
        // The element type is already vetted by `supported_type(&e.ty)` above.
        HirExprKind::Array { elements } => {
            let elem = array_element_type(&e.ty);
            elements
                .iter()
                .all(|el| ctx.storable(&elem, &el.ty) && child_supported(el))
        }
        // `Array<T>` and `FrozenArray<T>` are the same runtime handle, so both
        // index through `willow_array_get`. `Range<i64>` also
        // spells a read this way but is not a handle at all, so it stays out.
        HirExprKind::Index { array, index } => {
            let indexable = matches!(array.ty, Type::Array(_))
                || matches!(
                    lir_collection(&array.ty),
                    Some((LirCollection::FrozenArray, _))
                );
            indexable
                && ctx.same_repr(&array_element_type(&array.ty), &e.ty)
                && child_supported(array)
                && child_supported(index)
        }
        // `new Class(args)` — explicit `Class__init` or the implicit memberwise
        // constructor.
        HirExprKind::New { class, args } => {
            let Some(layout) = ctx.class_layout_of(&e.ty) else {
                return false;
            };
            if !matches!(&e.ty, Type::Named(n) if n == class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, &class.to_string(), "init");
            let shape_ok = if (ctx.known_fn)(&mangled) {
                ctx.callable(&mangled, args, true)
            } else {
                // Memberwise: positional args fill the declared fields in order.
                args.len() == layout.len()
                    && layout
                        .iter()
                        .zip(args)
                        .all(|((_, fty), a)| ctx.storable(fty, &a.ty))
            };
            shape_ok && args.iter().all(child_supported)
        }
        // `Class { field: value,... }` — the given names must be exactly the
        // declared fields, each once. Matching only the COUNT would accept
        // `Point { x: 1, x: 2 }`, which the emitter would store twice into `x`
        // and leave `y` at its zero value.
        //
        // The type checker rejects this syntax in source today (E0847), so this
        // arm only guards the node's internal use; the check stays exact so the
        // predicate and the emitter cannot drift apart if it comes back.
        HirExprKind::ObjectLiteral { class, fields } => {
            let Some(layout) = ctx.class_layout_of(&e.ty) else {
                return false;
            };
            if !matches!(&e.ty, Type::Named(n) if n == class) || fields.len() != layout.len() {
                return false;
            }
            let mut seen: HashSet<&str> = HashSet::new();
            fields.iter().all(|(name, value)| {
                seen.insert(name.as_str())
                    && layout
                        .iter()
                        .find(|(n, _)| n == name)
                        .is_some_and(|(_, fty)| ctx.storable(fty, &value.ty))
                    && child_supported(value)
            })
        }
        // `object.field` on a simple class, or the two `i64` bounds of a
        // `Range<i64>`. Other receivers fail validation.
        HirExprKind::FieldAccess { object, field } => {
            if range_i64(&object.ty) {
                return matches!(field.as_str(), "start" | "end")
                    && e.ty == Type::I64
                    && child_supported(object);
            }
            ctx.class_layout_of(&object.ty).is_some_and(|l| {
                l.iter()
                    .find(|(n, _)| n == field)
                    .is_some_and(|(_, fty)| ctx.same_repr(fty, &e.ty))
            }) && child_supported(object)
        }
        // Array public calls use the source-selected owner. Only storage
        // operations resolve through the intrinsic table.
        HirExprKind::MethodCall {
            object,
            method,
            args,
            super_class,
        } => match &object.ty {
            Type::Array(_) | Type::FixedArray(_, _) => {
                let shape_ok = if let Some(owner) = super_class {
                    let Some(mangled) = ctx.resolve_class_method(owner, method) else {
                        return false;
                    };
                    ctx.callable(&mangled, args, true)
                        && ctx.fn_types.get(&mangled).is_some_and(
                            |ty| matches!(ty, Type::Fn(_, ret) if ctx.repr_compatible(ret, &e.ty)),
                        )
                } else {
                    let arg_types: Vec<_> = args.iter().map(|arg| arg.ty.clone()).collect();
                    intrinsics::resolve(&object.ty, method, args.len()).is_some_and(|resolution| {
                        let result = resolution.return_type(|index| arg_types.get(index).cloned());
                        ctx.same_repr(&result, &e.ty)
                            && flat_intrinsic_supported(
                                resolution.intrinsic,
                                &object.ty,
                                &arg_types,
                                &e.ty,
                                ctx,
                            )
                    })
                };
                shape_ok && child_supported(object) && args.iter().all(child_supported)
            }
            // The value-taking `Option`/`Result` methods.
            // Checked before the collection arm because both receivers are
            // `Type::Generic`; `lir_collection` never matches an enum, so the
            // two sets cannot overlap.
            Type::Generic(..) if ctx.supported_enum_type(&object.ty) => {
                let arg_tys: Vec<Type> = args.iter().map(|a| a.ty.clone()).collect();
                option_result_method(&object.ty, method, &arg_tys)
                    .is_some_and(|ret| ctx.same_repr(&ret, &e.ty))
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            Type::Generic(_, type_args)
                if builtin_types::unary_arg(&object.ty, B::Channel).is_some() =>
            {
                let [elem] = type_args.as_slice() else {
                    return false;
                };
                let shape_ok = match method.as_str() {
                    "send" => {
                        args.len() == 1 && e.ty == Type::Void && ctx.storable(elem, &args[0].ty)
                    }
                    "recv" => args.is_empty() && ctx.repr_compatible(&e.ty, elem),
                    "recv_opt" => {
                        args.is_empty()
                            && ctx.repr_compatible(&e.ty, &B::Option.apply(vec![elem.clone()]))
                    }
                    "close" => args.is_empty() && e.ty == Type::Void,
                    _ => false,
                };
                shape_ok && child_supported(object) && args.iter().all(child_supported)
            }
            // `Task<T>`/`JoinHandle<T>` cancellation. Both
            // read only the frame header the handle already points at, so
            // there is nothing to vet beyond the receiver and the result type.
            Type::Generic(_, _)
                if builtin_types::resolve(&object.ty)
                    .is_some_and(|resolved| matches!(resolved.id, B::Task | B::JoinHandle))
                    && task_handle_method(&object.ty, method, args.len())
                        .is_some_and(|ret| ctx.same_repr(&ret, &e.ty)) =>
            {
                args.is_empty() && child_supported(object)
            }
            // The native-blocking cells. Each accessor is
            // one runtime call over a word-based ABI, so beyond the receiver
            // there is only the stored value to vet — and it is vetted against
            // the CELL's element type in both directions, because the emitter
            // coerces into a word on the way in and out of one on the way out.
            Type::Generic(..) if blocking_cell(&object.ty).is_some() => {
                let (_, elem) = blocking_cell(&object.ty).expect("guarded by the arm");
                let Some((_, ret)) = blocking_cell_method(&object.ty, method, args.len()) else {
                    return false;
                };
                ctx.same_repr(&ret, &e.ty)
                    && args.iter().all(|a| ctx.same_repr(elem, &a.ty))
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // The builtin collections. `get` yields an `Option<V>`, which the
            // walker represents as of.
            Type::Generic(name, _) if !(ctx.is_interface)(name) => {
                let Some((kind, targs)) = lir_collection(&object.ty) else {
                    return false;
                };
                let shape_ok = match (kind, method.as_str()) {
                    // A frozen array answers `len` and nothing else; its reads
                    // are `Index`, handled above.
                    (LirCollection::FrozenArray, "len") => args.is_empty() && e.ty == Type::I64,
                    (LirCollection::Map | LirCollection::FrozenMap, "len") => {
                        args.is_empty() && e.ty == Type::I64
                    }
                    (LirCollection::Map | LirCollection::FrozenMap, "contains") => {
                        args.len() == 1
                            && e.ty == Type::Bool
                            && ctx.same_repr(&targs[0], &args[0].ty)
                    }
                    // `get` yields `Option<V>` over the map's OWN value type —
                    // checked rather than assumed, because the runtime picks
                    // the option representation from that type and the walker
                    // passes the choice across the ABI.
                    (LirCollection::Map | LirCollection::FrozenMap, "get") => {
                        args.len() == 1
                            && ctx.same_repr(&targs[0], &args[0].ty)
                            && matches!(&e.ty, Type::Generic(..)
                                if builtin_types::unary_arg(&e.ty, B::Option) == Some(&targs[1]))
                            && ctx.supported_type(&e.ty)
                    }
                    (LirCollection::Map, "insert") => {
                        args.len() == 2
                            && ctx.same_repr(&targs[0], &args[0].ty)
                            && ctx.storable(&targs[1], &args[1].ty)
                    }
                    // Rendered in the runtime, which knows only the four
                    // scalar/string kinds — the former AST path passed `0` for
                    // anything else, which would render a pointer as an `i64`.
                    // Both halves are vetted: an admitted KEY always has a kind
                    // (`map_key_supported` allows only those four), but the
                    // check is written out so the emitter's `expect` stays
                    // honest if either subset moves.
                    (LirCollection::Map, "toString") => {
                        args.is_empty()
                            && e.ty == Type::String
                            && collection_elem_kind(&targs[0]).is_some()
                            && collection_elem_kind(&targs[1]).is_some()
                    }
                    // `freeze` copies into a `FrozenMap<K, V>` over the same
                    // pair, for the same reason `Array::freeze` checks its own.
                    (LirCollection::Map, "freeze") => {
                        args.is_empty()
                            && matches!(lir_collection(&e.ty), Some((LirCollection::FrozenMap, a))
                                if a == targs)
                    }
                    _ => false,
                };
                shape_ok && child_supported(object) && args.iter().all(child_supported)
            }
            // The cancellation handles. `cancel`, `child`
            // and `is_cancelled` read or write the handle alone; `attach`/`add`
            // additionally take a task and hand the same handle back, so the
            // result type is the argument's and both have to be admitted.
            Type::Named(_) if cancellation_handle(&object.ty).is_some() => {
                let Some((_, ret)) = cancellation_method(&object.ty, method, args) else {
                    return false;
                };
                ctx.same_repr(&ret, &e.ty)
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // The atomic cells. Every operation is one word
            // read or write inside a cell that already exists, so beyond the
            // receiver there is only the operand to vet — and both it and the
            // result must be the cell's own width, because the emitter passes
            // them through untouched.
            Type::Named(_) if atomic_cell(&object.ty).is_some() => {
                let cell = atomic_cell(&object.ty).expect("guarded by the arm");
                let Some((_, ret)) = atomic_method(&object.ty, method, args.len()) else {
                    return false;
                };
                ctx.same_repr(&ret, &e.ty)
                    && args.iter().all(|a| a.ty == cell.word())
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // Virtual dispatch through an interface box. The
            // receiver's box carries the vtable, so — unlike a class receiver —
            // this needs no knowledge of the concrete class, only that the
            // interface really declares the method at a known slot.
            Type::Named(iface) if (ctx.is_interface)(iface) => {
                let Some(sig) = (ctx.iface_method)(&object.ty, method) else {
                    return false;
                };
                // A `Self`-returning method yields a concrete object of the
                // receiver's own class, which the emitter re-boxes with the
                // receiver's vtable — so the result is the receiver's interface
                // and nothing else.
                let ret_ok = if matches!(&sig.ret, Type::Named(n) if n == &TypeId::local("Self")) {
                    matches!(&e.ty, Type::Named(n) if n == iface)
                } else {
                    ctx.same_repr(&sig.ret, &e.ty)
                };
                ret_ok
                    && sig.params.len() == args.len()
                    && sig.params.iter().all(|p| ctx.supported_type(p))
                    && sig
                        .params
                        .iter()
                        .zip(&sig.modes)
                        .zip(args)
                        .all(|((p, mode), a)| {
                            matches!(mode, ParamMode::Reference { .. })
                                == matches!(a.kind, HirExprKind::ReferenceArg { .. })
                                && ctx.storable(p, &a.ty)
                        })
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            Type::Generic(iface, _) if (ctx.is_interface)(iface) => {
                let Some(sig) = (ctx.iface_method)(&object.ty, method) else {
                    return false;
                };
                let ret_ok = if matches!(&sig.ret, Type::Named(n) if n == &TypeId::local("Self")) {
                    sig.ret == e.ty
                } else {
                    ctx.same_repr(&sig.ret, &e.ty)
                };
                ret_ok
                    && sig.params.len() == args.len()
                    && sig.params.iter().all(|p| ctx.supported_type(p))
                    && sig
                        .params
                        .iter()
                        .zip(&sig.modes)
                        .zip(args)
                        .all(|((p, mode), a)| {
                            matches!(mode, ParamMode::Reference { .. })
                                == matches!(a.kind, HirExprKind::ReferenceArg { .. })
                                && ctx.storable(p, &a.ty)
                        })
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // Scalar `toString` — the only builtin method with a primitive
            // receiver. Which intrinsic a call denotes is answered by the
            // shared table rather than re-derived from the method name here,
            // so the walker cannot disagree with the checker about what the
            // call means or what it produces.
            Type::I64 | Type::F64 | Type::Bool | Type::String => {
                matches!(
                    scalar_to_string(&object.ty, method, args),
                    Some(ret) if ret == e.ty
                ) && child_supported(object)
            }
            // A class receiver. The implementation is resolved through the
            // receiver's ancestry rather than assumed to be declared on the
            // receiver's own class, so an INHERITED method is in the subset
            //; whether the emitted call is direct or goes
            // through the descriptor slot is decided later, by the same
            // shared `plan_virtual_call` helper.
            Type::Named(class) if ctx.supported_class(class) => {
                let Some(mangled) = ctx.resolve_class_method(class, method) else {
                    return false;
                };
                ctx.callable(&mangled, args, true)
                    && ctx.fn_types.get(&mangled).is_some_and(
                        |t| matches!(t, Type::Fn(_, ret) if ctx.repr_compatible(ret, &e.ty)),
                    )
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            _ => false,
        },
        // Two unrelated things share this node. `Enum::Variant` with no
        // payload, and the bare unqualified form the checker resolved to one
        // (`Red`, `None`), lower to a static property read —
        // and so does a real `Class::property`, which loads from module data
        //.
        HirExprKind::StaticField { class, field } => {
            if ctx.known_modules.contains_key(&class.to_string()) {
                return false;
            }
            if ctx.is_enum(&class.to_string()) {
                return ctx.supported_enum_type(&e.ty)
                    && ctx.enum_instance(&e.ty).is_some_and(|(name, def)| {
                        name == *class && def.variant(field).is_some_and(|v| v.payloads.is_empty())
                    });
            }
            // `Self::prop` inside a method resolves against the enclosing
            // class, exactly as `emit_static_field_read` resolves it
            //.
            (ctx.static_field)(ctx.resolved_class(&class.to_string()), field)
                .is_some_and(|ty| ctx.supported_type(&ty) && ctx.repr_compatible(&e.ty, &ty))
        }
        // `Class::method(args)` — a static method of a simple class, or an enum
        // variant construction. A module call (`math::add`) and a builtin
        // namespace (`fs`, `env`) spell themselves the same way and still need
        // dedicated builtin dispatch.
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => {
            // `Self::method(..)` inside a class body names the class the body
            // is DECLARED in, which is what the emitter resolves it to as well
            //. Resolved once, here, so every test below —
            // the builtin spellings, the enum path, the class path and the
            // symbol they mangle — sees the one name.
            let class_spelling = class.to_string();
            let class = ctx.resolved_class(&class_spelling);
            // `Map::new` is the one builtin constructor in the subset. The
            // result type decides, not the spelling, so a user class called
            // `Map` cannot reach `willow_map_new`: it would have a `Named` type
            // and fall through to the class path below. The empty map is either
            // already instantiated (and then vetted by `supported_type` above)
            // or carries the untyped `Map<Void, Void>` the checker gives it.
            if class == "Map" && method == "new" && args.is_empty() {
                return matches!(lir_collection(&e.ty), Some((LirCollection::Map, _)));
            }
            if class == "Channel"
                && matches!(method.as_str(), "new" | "with_capacity")
                && builtin_types::unary_arg(&e.ty, B::Channel).is_some()
                && ctx.supported_type(&e.ty)
            {
                return (method == "new" && args.is_empty())
                    || (method == "with_capacity"
                        && args.len() == 1
                        && args[0].ty == Type::I64
                        && child_supported(&args[0]));
            }
            // `AtomicI64::new(i64)` / `AtomicBool::new(bool)`
            //. The RESULT type picks the cell, and the
            // operand is vetted against that cell's word: the emitter passes it
            // to the runtime unchanged, so a mismatch would be a
            // reinterpretation rather than a coercion. Any other spelling —
            // a user `static fn new` that happens to return a cell — falls
            // through to the class path below and is compiled as the call it
            // is; the emitter keys on the same class name so the two stay in
            // step.
            if let Some(cell) = atomic_cell(&e.ty)
                && method == "new"
                && class == cell.class_name()
            {
                return matches!(args.as_slice(), [a] if a.ty == cell.word())
                    && child_supported(&args[0]);
            }
            // `BlockingCell<T>::new(v)` / `BlockingRwCell<T>::new(v)`
            //. Keyed on the class as well as the result
            // type, for the same reason the atomic constructor above is. The
            // initial value is a STORE into the cell's element slot, but the
            // emitter coerces it to a word rather than boxing it, so this is
            // `assignable_repr` rather than `storable`.
            if let Some((kind, elem)) = blocking_cell(&e.ty)
                && method == "new"
                && class == kind.class_name()
            {
                return matches!(args.as_slice(), [a] if ctx.same_repr(elem, &a.ty))
                    && child_supported(&args[0]);
            }
            // `Mutex<T>::new(v)` / `RwLock<T>::new(v)`.
            // Same shape as the blocking cells above — the handle is a
            // one-word cell built from an initial value — and keyed the same
            // way, on the class as well as the result type.
            if let Some((_, protected)) = scheduler_lock(&e.ty)
                && method == "new"
                && matches!(class, "Mutex" | "RwLock")
            {
                return matches!(args.as_slice(), [a] if ctx.same_repr(protected, &a.ty))
                    && child_supported(&args[0]);
            }
            // `CancellationToken::new` / `TaskScope::new`
            //. Keyed on the class as well as the result
            // type, for the same reason the cell constructor above is.
            if let Some(handle) = cancellation_handle(&e.ty)
                && method == "new"
                && class == handle.class_name()
            {
                return args.is_empty();
            }
            // The `env`, `fs` and `net` builtin namespaces and the two `f64::`
            // calls: fixed-signature
            // runtime calls, admitted from the same table the emitter
            // dispatches on, so a call the walker accepts is one it can emit.
            if let Some(entry) =
                namespace_builtin_call(ctx.known_modules, ctx.builtin_module_aliases, class, method)
            {
                return entry.params.len() == args.len()
                    && entry
                        .params
                        .iter()
                        .zip(args)
                        .all(|(slot, a)| ctx.storable(slot, &a.ty))
                    && ctx.supported_type(&e.ty)
                    && ctx.repr_compatible(&e.ty, &entry.ret)
                    && args.iter().all(child_supported);
            }
            // A call into an imported user module (`math::add(1, 2)`), which
            // HIR spells as a static call whose "class" is the module's access
            // name. It is a FREE function: its symbol is the
            // module item symbol, and it takes no hidden `self` — which is why
            // it cannot fall through to the class path below, where a module
            // name is not a class and `supported_class` would refuse it.
            if let Some(module_prefix) = ctx.known_modules.linker_prefix(class) {
                let mangled = module_item_symbol(module_prefix, method);
                return ctx.callable(&mangled, args, false)
                    && ctx.fn_types.get(&mangled).is_some_and(
                        |t| matches!(t, Type::Fn(_, ret) if ctx.same_repr(ret, &e.ty)),
                    )
                    && args.iter().all(child_supported);
            }
            // `Enum::Variant(payload…)`, and the qualified fieldless form,
            // which HIR also spells as a zero-argument static call. The
            // payloads are STORE positions (the emitter coerces each one into
            // its declared slot), so `storable` is the right test, not
            // `assignable_repr`.
            if ctx.is_enum(&class.to_string()) {
                if !ctx.supported_enum_type(&e.ty) {
                    return false;
                }
                // The result type is what instantiates the variant, so
                // `Option::Some(1)` typed `Option<i64>` vets its payload
                // against `i64` rather than the declaration's `T`.
                let Some((name, def)) = ctx.enum_instance(&e.ty) else {
                    return false;
                };
                let Some(variant) = def.variant(method) else {
                    return false;
                };
                return name == TypeId::from_source_name(class)
                    && variant.payloads.len() == args.len()
                    && variant
                        .payloads
                        .iter()
                        .zip(args)
                        .all(|(slot, a)| ctx.storable(slot, &a.ty))
                    && args.iter().all(child_supported);
            }
            if !ctx.supported_class(class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, class, method);
            ctx.callable(&mangled, args, true)
                && ctx
                    .fn_types
                    .get(&mangled)
                    .is_some_and(|t| matches!(t, Type::Fn(_, ret) if ctx.same_repr(ret, &e.ty)))
                && args.iter().all(child_supported)
        }
        // `start..end` as a VALUE. The bounds are the two
        // words of the object the emitter allocates, so both have to be `i64`
        // and the node's own type has to be the range it builds — a retyped
        // node must not make the walker store two words under another shape.
        HirExprKind::Range { start, end } => {
            range_i64(&e.ty)
                && start.ty == Type::I64
                && end.ty == Type::I64
                && child_supported(start)
                && child_supported(end)
        }
        // `expr?` on an `Option`/`Result`. Two halves:
        //
        // * The SUCCESS value is this node's own type, which must be the
        //   operand's first type argument — the payload the emitter reads out
        //   of word 1 (or out of the niche pointer itself). Checked rather than
        //   assumed so a lowering that ever retyped the node cannot make the
        //   walker reinterpret a payload word.
        // * The PROPAGATED value is the enclosing function's own return value.
        //   `lir_rejection_reason` has already vetted `f.return_type` as a
        //   supported type, and the type checker guarantees it is the matching
        //   `Option`/`Result`, so no further test is possible here — this arm
        //   sees the operand, not the function.
        //
        HirExprKind::TryPropagate { inner } => {
            let Some(resolved) = builtin_types::resolve(&inner.ty) else {
                return false;
            };
            matches!(resolved.id, B::Option | B::Result)
                && ctx.supported_enum_type(&inner.ty)
                && resolved.args.first().is_some_and(|payload| match payload {
                    // `Result<void, E>` — `f?;` in statement position, which
                    // is how every `net::` and `fs::` write is spelled
                    //. The success arm has no payload to
                    // read: `enum_instance` normalizes the `void` away, so the
                    // `Ok` object is the tag word alone and a word-1 load would
                    // read past it. The emitter loads nothing in that case, and
                    // the node's own `void` type is what says so.
                    Type::Void => resolved.id == B::Result && e.ty == Type::Void,
                    _ => ctx.same_repr(payload, &e.ty),
                })
                && child_supported(inner)
        }
        HirExprKind::Await { inner }
            if builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                && !lir_suspends_here(e) =>
        {
            builtin_types::unary_arg(&inner.ty, B::Future) == Some(&e.ty)
                && ctx.supported_type(&e.ty)
                && child_supported(inner)
        }
        HirExprKind::Await { .. } => match lir_await_site(e, ctx.cooperative_leaves) {
            Some(LirAwaitSite::Sleep(millis)) => child_supported(millis),
            Some(LirAwaitSite::Yield) => true,
            // `await f(..)` on a cooperative leaf. The constructor call is
            // vetted exactly as a synchronous call to `f` would be — same
            // parameter table, same argument rules — and the awaited value is
            // read back out of the callee frame's RESULT slot at this node's
            // own type.
            Some(LirAwaitSite::LeafCall { callee, args, .. }) => {
                ctx.callable(&callee.to_string(), args, false)
                    && ctx.supported_type(&e.ty)
                    && args.iter().all(child_supported)
            }
            None => false,
        },
        HirExprKind::Select { cases } => e.ty == Type::Void && supported_select(cases, ctx, names),
    }
}

/// The source-shaped name of a reference place. See [`lir_reference_place_kind`]
/// for why this must agree with the AST spelling exactly.
#[cfg(test)]
pub(super) fn lir_reference_place_name(place: &HirExpr) -> String {
    let mut current = place;
    let mut suffixes = Vec::new();
    let mut out = loop {
        match &current.kind {
            HirExprKind::Var(name) => break name.clone(),
            HirExprKind::FieldAccess { object, field } => {
                suffixes.push((Some(field.as_str()), None));
                current = object;
            }
            HirExprKind::Index { array, index } => {
                suffixes.push((None, Some(index.as_ref())));
                current = array;
            }
            _ => break "<expression>".to_string(),
        }
    };
    for (field, index) in suffixes.into_iter().rev() {
        if let Some(field) = field {
            out.push('.');
            out.push_str(field);
        } else if let Some(index) = index {
            out.push('[');
            out.push_str(&lir_reference_index_name(index));
            out.push(']');
        }
    }
    out
}

#[cfg(test)]
fn lir_reference_index_name(index: &HirExpr) -> String {
    match &index.kind {
        HirExprKind::Int(value) => value.to_string(),
        HirExprKind::Var(name) => name.clone(),
        _ => "<expr>".to_string(),
    }
}

/// Whether `place` has stable address semantics for a `&`/`&mut` call
/// argument: a local/reference parameter, a class field, or an array element.
#[cfg(test)]
fn supported_reference_place<'n>(
    place: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    child_supported: &impl Fn(&'n HirExpr) -> bool,
) -> bool {
    match &place.kind {
        HirExprKind::Var(name) => names
            .get(name.as_str())
            .is_some_and(|bound| ctx.same_repr(bound, &place.ty)),
        HirExprKind::FieldAccess { object, field } => {
            ctx.class_layout_of(&object.ty).is_some_and(|layout| {
                layout
                    .iter()
                    .find(|(name, _)| name == field)
                    .is_some_and(|(_, ty)| ctx.same_repr(ty, &place.ty))
            }) && child_supported(object)
        }
        HirExprKind::Index { array, index } => {
            matches!(&array.ty, Type::Array(elem) if ctx.same_repr(elem, &place.ty))
                && index.ty == Type::I64
                && child_supported(array)
                && child_supported(index)
        }
        _ => false,
    }
}
