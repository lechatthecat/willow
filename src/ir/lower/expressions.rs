//! Expression lowering and pattern binding.
use super::*;

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
pub(super) fn lower_expr(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // Operator lowering checks/transcribes every operator, including the root.
    // Do not read and clone the root's type twice.
    let checked_type = if matches!(expr, Expr::Binary(_) | Expr::Unary(_)) {
        None
    } else {
        required_expr_type(expr.id(), expr.span(), ctx)?
    };
    if ctx.tables.checked
        && matches!(expr, Expr::Lambda(_))
        && !ctx
            .tables
            .lambda_captures
            .is_some_and(|facts| facts.contains_key(&expr.id()))
    {
        return Err(missing_fact(expr.span(), "lambda captures"));
    }
    if ctx.tables.checked
        && matches!(expr, Expr::StaticCall(_) | Expr::StaticField(_))
        && !ctx
            .tables
            .static_call_classes
            .is_some_and(|facts| facts.contains_key(&expr.id()))
    {
        return Err(missing_fact(expr.span(), "static target"));
    }
    let mut lowered = lower_expr_inner(expr, ctx)?;
    lowered.ty = ctx
        .normalize(&checked_type.unwrap_or_else(|| lowered.ty.to_source()))
        .into();
    match &mut lowered.kind {
        HirExprKind::StaticCall { class, .. } | HirExprKind::StaticField { class, .. } => {
            let source = if *class == crate::semantic::ids::TypeId::local("Self") {
                ctx.current_class
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| class.to_string())
            } else {
                class.to_string()
            };
            if let Type::Named(name) = &ctx.normalize(&Type::Named(source)) {
                *class = name.as_str().into();
            }
        }
        _ => {}
    }
    // Construction records the identity being allocated, not the source's
    // import alias. Keep it identical to the canonical result type.
    if let crate::semantic::ids::SemanticType::Named(identity) = &lowered.ty {
        match &mut lowered.kind {
            HirExprKind::New { class, .. } | HirExprKind::ObjectLiteral { class, .. } => {
                *class = *identity
            }
            _ => {}
        }
    }
    Ok(lowered)
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_expr_inner(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    match expr {
        Expr::Integer(n, span, _) => Ok(lit(HirExprKind::Int(*n), Type::I64, *span)),
        Expr::Float(f, span, _) => Ok(lit(HirExprKind::Float(*f), Type::F64, *span)),
        Expr::Bool(b, span, _) => Ok(lit(HirExprKind::Bool(*b), Type::Bool, *span)),
        Expr::String(s, span, _) => Ok(lit(HirExprKind::Str(s.clone()), Type::String, *span)),
        Expr::Var(name, span, _) => lower_var_expr(expr, name, *span, ctx),
        Expr::Binary(_) | Expr::Unary(_) => lower_operator_tree(expr, ctx),
        Expr::Call(c) => lower_call_expr(c, ctx),
        Expr::Print(inner, newline, span, _) => lower_print_expr(inner, *newline, *span, ctx),
        Expr::ArrayLiteral(elements, span, _) => {
            lower_array_literal_expr(expr, elements, *span, ctx)
        }
        Expr::Index(array, index, span, _) => lower_index_expr(array, index, *span, ctx),
        Expr::Ternary(t) => lower_ternary_expr(t, ctx),
        Expr::New(n) => lower_new_expr(n, ctx),
        Expr::FieldAccess(object, field, span, _) => {
            lower_field_access_expr(expr, object, field, *span, ctx)
        }
        Expr::MethodCall(m) => lower_method_call_expr(m, ctx),
        Expr::ObjectLiteral(o) => lower_object_literal_expr(o, ctx),
        Expr::StaticField(s) => lower_static_field_expr(s, ctx),
        Expr::StaticCall(s) => lower_static_call_expr(s, ctx),
        Expr::Range(r) => lower_range_expr(r, ctx),
        Expr::Select(select) => lower_select_expr(select, ctx),
        Expr::Await(a) => lower_await_expr(a, ctx),
        Expr::TryPropagate(inner, span, _) => lower_try_propagate_expr(inner, *span, ctx),
        Expr::Lambda(l) => lower_lambda_expr(l, ctx),
        Expr::Match(m) => lower_match(m, ctx),
    }
}

/// A missing checked fact is an invariant error, not permission to infer it.
fn required_expr_type(id: ExprId, span: Span, ctx: &LowerCtx) -> Result<Option<Type>, Diagnostic> {
    #[cfg(test)]
    REQUIRED_TYPE_READS.with(|reads| reads.set(reads.get() + 1));
    let ty = ctx.expr_type(&id);
    if ctx.tables.checked && ty.is_none() {
        return Err(missing_fact(span, "expression type"));
    }
    Ok(ty)
}

/// The literal one use of a module-level `const` stands for.
fn const_literal(value: &crate::parser::ast::ConstValue, span: Span) -> HirExpr {
    use crate::parser::ast::ConstValue;
    let kind = match value {
        ConstValue::Integer(n) => HirExprKind::Int(*n),
        ConstValue::Float(f) => HirExprKind::Float(*f),
        ConstValue::Bool(b) => HirExprKind::Bool(*b),
        ConstValue::String(s) => HirExprKind::Str(s.clone()),
    };
    lit(kind, value.ty(), span)
}

/// Each non-trivial arm of [`lower_expr_inner`] lives in its own function so a
/// nested expression costs one SMALL frame per level instead of one frame
/// holding every arm's locals at once: a debug build gives the match's frame
/// room for all of them, which cost ~63KB per level and overflowed a 2MiB
/// thread at around 30 levels of nesting. `inline(never)` keeps
/// that true in release builds, where the arms would otherwise be folded back
/// into one frame.
#[inline(never)]
fn lower_var_expr(
    expr: &Expr,
    name: &str,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    if let Some(binding) = ctx.lookup(name) {
        if binding.cell {
            return Ok(cell_read(binding, span));
        }
        return Ok(HirExpr {
            kind: HirExprKind::Var(binding.hir_name.to_string()),
            ty: binding.ty.clone().into(),
            span,
        });
    }
    // A bare fieldless unqualified variant (`None`, `Halt`) parses as a
    // variable; the checker resolves it against the expected enum and
    // records both the enum and the expression type.
    if let Some(enum_name) = ctx.tables.enum_variant_resolution(&expr.id()) {
        let ty = ctx
            .expr_type(&expr.id())
            .unwrap_or_else(|| Type::Named(enum_name.to_string()));
        return Ok(HirExpr {
            kind: HirExprKind::StaticField {
                class: enum_name.to_string().into(),
                field: name.to_string(),
            },
            ty: ty.into(),
            span,
        });
    }
    // A module-level `const`: inline its literal. Locals
    // were resolved above, so a binding of the same name still shadows it,
    // exactly as in the checker.
    if let Some(value) = ctx.tables.constant(name) {
        return Ok(const_literal(value, span));
    }
    // A named top-level function used as a value (`apply(10, double)`,
    // function value lowering). The checker types the bare identifier as the
    // function's `fn(...) -> ...`; taking that from the checker rather
    // than rebuilding it keeps the parameter modes and the return type
    // exactly what the call site was checked against. A function bound by an
    // item import is not in `fn_returns`; the back end binds its local name
    // to the imported symbol, as for a direct call.
    if (ctx.fn_returns.contains_key(name)
        || ctx
            .tables
            .symbols
            .is_some_and(|symbols| symbols.lookup_func(name).is_some()))
        && let Some(ty @ Type::Fn(..)) = ctx.expr_type(&expr.id())
    {
        return Ok(HirExpr {
            kind: HirExprKind::FnRef(name.into()),
            ty: ty.into(),
            span,
        });
    }
    Err(internal(
        span,
        format!("unbound variable `{name}` reached HIR lowering"),
    ))
}

/// Operator chains have no lexical boundaries. Keep their construction frames
/// on the heap, delegating other syntax to its context-sensitive lowering.
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_operator_tree(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    enum Step<'a> {
        Enter(&'a Expr),
        Binary(&'a BinaryExpr),
        Unary(&'a UnaryExpr),
    }
    let mut work = vec![Step::Enter(expr)];
    let mut values: Vec<HirExpr> = Vec::new();
    while let Some(step) = work.pop() {
        let mut value = match step {
            Step::Enter(Expr::Binary(binary)) => {
                work.push(Step::Binary(binary));
                work.push(Step::Enter(&binary.rhs));
                work.push(Step::Enter(&binary.lhs));
                continue;
            }
            Step::Enter(Expr::Unary(unary)) => {
                work.push(Step::Unary(unary));
                work.push(Step::Enter(&unary.expr));
                continue;
            }
            Step::Enter(other) => match lower_expr(other, ctx) {
                Ok(value) => {
                    values.push(value);
                    continue;
                }
                Err(error) => {
                    // HIR ownership drains partial results iteratively.
                    values.clear();
                    return Err(error);
                }
            },
            Step::Binary(binary) => {
                let rhs = values.pop().expect("lowered right operand");
                let lhs = values.pop().expect("lowered left operand");
                let ty = required_expr_type(binary.id, binary.span, ctx)?
                    .unwrap_or_else(|| binary_result_type(&binary.op, &lhs.ty.to_source()));
                HirExpr {
                    kind: HirExprKind::Binary {
                        op: binary.op.clone(),
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    ty: ty.into(),
                    span: binary.span,
                }
            }
            Step::Unary(unary) => {
                let operand = values.pop().expect("lowered unary operand");
                let ty = required_expr_type(unary.id, unary.span, ctx)?
                    .map(Into::into)
                    .unwrap_or_else(|| match unary.op {
                        UnaryOp::Neg => operand.ty.clone(),
                        // `!` is bitwise on `i64` and logical on `bool`.
                        UnaryOp::Not if operand.ty == Type::I64 => Type::I64,
                        UnaryOp::Not => Type::Bool,
                    });
                HirExpr {
                    kind: HirExprKind::Unary {
                        op: unary.op.clone(),
                        operand: Box::new(operand),
                    },
                    ty,
                    span: unary.span,
                }
            }
        };
        // The outer lower_expr normalizes the root once, just as before.
        if !work.is_empty() {
            value.ty = ctx.normalize(&value.ty.to_source()).into();
        }
        values.push(value);
    }
    Ok(values.pop().expect("lowered operator root"))
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_call_expr(c: &CallExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let args = lower_value_args(&c.args, ctx)?;
    if let Some(binding) = ctx.lookup(&c.callee).filter(|binding| binding.cell) {
        let (Type::Fn(_, ret) | Type::Closure(_, ret)) = &binding.ty else {
            return Err(internal(c.span, "non-callable capture cell".to_string()));
        };
        return Ok(HirExpr {
            kind: HirExprKind::Invoke {
                name: c.callee.clone().into(),
                callee: Box::new(cell_read(binding, c.span)),
                args,
            },
            ty: (**ret).clone().into(),
            span: c.span,
        });
    }
    // An unqualified enum-variant construction (`Ok(42)` in an
    // expected-enum position) parses as a call; the checker records
    // which enum it resolved to.
    if let Some(enum_name) = ctx.tables.enum_variant_resolution(&c.id) {
        // Prefer the checker's recorded type (it carries generic type
        // arguments, e.g. `Result<i64, String>` for `Ok(42)`).
        let ty = match ctx.expr_type(&c.id) {
            Some(ty) => ty,
            None => enum_variant_construction_type(ctx.enums, enum_name, &args, c.span)?,
        };
        return Ok(HirExpr {
            kind: HirExprKind::StaticCall {
                class: enum_name.clone().into(),
                method: c.callee.clone(),
                args,
            },
            ty: ty.into(),
            span: c.span,
        });
    }
    // A local fn-typed variable shadows a free function; its call is an
    // indirect call typed by the variable's `fn(..) -> R`, and it is
    // named by the binding's HIR name.
    let indirect = ctx.lookup(&c.callee).and_then(|b| match &b.ty {
        Type::Fn(_, ret) | Type::Closure(_, ret) => Some((b.hir_name.clone(), (**ret).clone())),
        _ => None,
    });
    let callee = match &indirect {
        Some((name, _)) => name.clone(),
        None => {
            // The later operand pass sees all locals at once. Reserve this
            // free callee's spelling so a subsequent local binding cannot
            // retroactively turn this call into an indirect local call.
            if !ctx.binds_seen.contains_key(&c.callee) {
                ctx.binds_seen.insert(c.callee.clone(), 1);
            }
            c.callee.clone()
        }
    };
    let ty = indirect
        .map(|(_, ret)| ret)
        .or_else(|| ctx.fn_returns.get(&c.callee).cloned())
        // A function bound by an ITEM import (`import calc::add;`) is a
        // plain unqualified call in this file, but it is not one of the
        // program's own items, so `fn_returns` has never heard of it. The checker did resolve
        // the import and typed
        // the call, so its type is what makes the call lowerable at
        // all. The back end binds this same local name to the mangled
        // symbol of the module THIS unit imported, rebinding it before
        // each unit's bodies, so the call the type came from is the
        // call that gets emitted.
        .or_else(|| ctx.expr_type(&c.id))
        .ok_or_else(|| {
            internal(
                c.span,
                format!(
                    "call to unknown function `{}` reached HIR lowering",
                    c.callee
                ),
            )
        })?;
    Ok(HirExpr {
        kind: HirExprKind::Call {
            callee: callee.into(),
            args,
        },
        ty: ty.into(),
        span: c.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_print_expr(
    inner: &Expr,
    newline: bool,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let value = lower_expr(inner, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::Print {
            value: Box::new(value),
            newline,
        },
        ty: Type::Void,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_array_literal_expr(
    expr: &Expr,
    elements: &[Expr],
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let mut lowered = Vec::with_capacity(elements.len());
    for element in elements {
        lowered.push(lower_expr(element, ctx)?);
    }
    // The checker owns contextual typing for every literal, not only empty
    // ones. Inferring again from element 0 loses interface storage/boxing.
    // The first-element fallback supports callers lowering unchecked syntax.
    let ty = match ctx.expr_type(&expr.id()) {
        Some(recorded @ Type::Array(_)) => recorded.into(),
        _ => match lowered.first() {
            Some(first) => Type::Array(Box::new(first.ty.clone())),
            None => {
                return Err(unsupported(
                    span,
                    "empty array literal with no recorded element type",
                ));
            }
        },
    };
    Ok(HirExpr {
        kind: HirExprKind::Array { elements: lowered },
        ty,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_index_expr(
    array: &Expr,
    index: &Expr,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let array = lower_expr(array, ctx)?;
    let index = lower_expr(index, ctx)?;
    // A `FrozenArray<T>` is the same runtime handle as the `Array<T>` it
    // was frozen from, and `arr[i]` reads it the same way, so both lower
    // to one `Index` node. `Range<i64>` also spells a
    // read this way but is not a handle at all, so it stays unsupported.
    let element = match &array.ty {
        Type::Array(element) => (**element).clone(),
        ty => match builtin_types::unary_arg(ty, B::FrozenArray) {
            Some(element) => element.clone(),
            None => return Err(unsupported(span, "index of a non-array value")),
        },
    };
    let ty = element;
    Ok(HirExpr {
        kind: HirExprKind::Index {
            array: Box::new(array),
            index: Box::new(index),
        },
        ty,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_ternary_expr(t: &TernaryExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let condition = lower_expr(&t.condition, ctx)?;
    let then_expr = lower_expr(&t.then_expr, ctx)?;
    let else_expr = lower_expr(&t.else_expr, ctx)?;
    // A diverging arm contributes no value to the result. If both diverge,
    // the result remains Never. Otherwise the checker's type is
    // authoritative: branches of distinct classes flowing into an interface
    // take that interface.
    let ty = if then_expr.ty == Type::Never {
        else_expr.ty.clone()
    } else {
        then_expr.ty.clone()
    };
    let ty = match ctx.expr_type(&t.id) {
        Some(checked) if ty != Type::Never => checked.into(),
        _ => ty,
    };
    Ok(HirExpr {
        kind: HirExprKind::Ternary {
            condition: Box::new(condition),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        },
        ty,
        span: t.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_new_expr(n: &NewExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let args = lower_value_args(&n.args, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::New {
            class: n.class_name.clone().into(),
            args,
        },
        ty: Type::Named(n.class_name.clone().into()),
        span: n.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_field_access_expr(
    expr: &Expr,
    object: &Expr,
    field: &str,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let object = lower_expr(object, ctx)?;
    let ty = {
        match class_name_of(&object.ty.to_source())
            .and_then(|class| ctx.classes.field_type(class, field))
        {
            Some(ty) => ty,
            None => ctx
                .expr_type(&expr.id())
                .ok_or_else(|| unsupported(span, "field not found on receiver"))?,
        }
    };
    Ok(HirExpr {
        kind: HirExprKind::FieldAccess {
            object: Box::new(object),
            field: field.to_string(),
        },
        ty: ty.into(),
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_method_call_expr(m: &MethodCallExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // `super.method(args)`: the receiver is `self`, and the checker typed the
    // `super` receiver as the base class whose implementation is called.
    if let Expr::Var(name, span, id) = &m.object
        && name == "super"
        && ctx.lookup("super").is_none()
    {
        let base = ctx.expr_type(id);
        let (Some(Type::Named(base)), Some(ty)) = (&base, ctx.expr_type(&m.id)) else {
            return Err(unsupported(m.span, "unresolved `super` method call"));
        };
        let Some(binding) = ctx.lookup("self") else {
            return Err(unsupported(*span, "`super` outside an instance method"));
        };
        let object = HirExpr {
            kind: HirExprKind::Var(binding.hir_name.to_string()),
            ty: binding.ty.clone().into(),
            span: *span,
        };
        // The checker types `super` as the base written in this unit (an item
        // import's bare `Parcel`); the class tables key it canonically.
        let base = match ctx.tables.normalize(&Type::Named(base.clone())) {
            Type::Named(ref canonical) => canonical.clone(),
            _ => base.clone(),
        };
        let args = lower_value_args(&m.args, ctx)?;
        return Ok(HirExpr {
            kind: HirExprKind::MethodCall {
                object: Box::new(object),
                method: m.method.clone(),
                args,
                super_class: Some(base.as_str().into()),
            },
            ty: ty.into(),
            span: m.span,
        });
    }
    let object = lower_expr(&m.object, ctx)?;
    let ty = if let Some(ty) = builtin_method_type(&object.ty.to_source(), &m.method) {
        ty
    } else if let Some(ty) = class_name_of(&object.ty.to_source())
        .and_then(|class| ctx.classes.method_type(class, &m.method))
    {
        ty
    } else {
        // Checker authority: interface methods, generic receivers,
        // Option/Result methods, and anything else it typed.
        ctx.expr_type(&m.id)
            .ok_or_else(|| unsupported(m.span, "method not found on receiver"))?
    };
    let args = lower_value_args(&m.args, ctx)?;
    let owner = ctx
        .tables
        .static_call_classes
        .and_then(|owners| owners.get(&m.id))
        .cloned();
    let forward_length = owner.as_ref().is_some_and(|owner| {
        ctx.tables
            .symbols
            .and_then(|symbols| symbols.lookup_class(&owner.to_string()))
            .and_then(|class| class.methods.get(&m.method))
            .is_some_and(|method| {
                method.array_body_optimization == crate::parser::ast::ArrayBodyOptimization::Length
            })
    });
    Ok(HirExpr {
        kind: HirExprKind::MethodCall {
            object: Box::new(object),
            method: if forward_length {
                "__abi_len".into()
            } else {
                m.method.clone()
            },
            args,
            super_class: if forward_length { None } else { owner },
        },
        ty: ty.into(),
        span: m.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_object_literal_expr(
    o: &ObjectLiteralExpr,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let mut fields = Vec::with_capacity(o.fields.len());
    for f in &o.fields {
        fields.push((f.name.clone(), lower_expr(&f.value, ctx)?));
    }
    Ok(HirExpr {
        kind: HirExprKind::ObjectLiteral {
            class: o.class.clone().into(),
            fields,
        },
        ty: Type::Named(o.class.clone().into()),
        span: o.span,
    })
}

#[inline(never)]
fn lower_static_field_expr(s: &StaticFieldExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // `config::LIMIT` names a module's `const` before any class,
    // matching the checker's precedence.
    if let Some(value) = ctx.tables.module_constant(&s.class, &s.field) {
        return Ok(const_literal(value, s.span));
    }
    // `Enum::Variant` (fieldless) parses like a static property read.
    let resolved = ctx.tables.static_call_class(&s.id, &s.class);
    let class = if resolved == "Self" {
        ctx.current_class.as_deref().unwrap_or(&resolved)
    } else {
        &resolved
    };
    let variant_ty = enum_variant_value_type(ctx.enums, class, &s.field, true);
    let ty = variant_ty
        .or_else(|| ctx.classes.static_field_type(class, &s.field))
        // Checker authority, exactly as the static-CALL arm below uses
        // it: `Self::prop` inside a class body names a class the
        // registry has no entry for, and the checker has already
        // resolved and typed the read.
        .or_else(|| ctx.expr_type(&s.id))
        .ok_or_else(|| unsupported(s.span, "static property not found"))?;
    Ok(HirExpr {
        kind: HirExprKind::StaticField {
            class: class.into(),
            field: s.field.clone(),
        },
        ty: ty.into(),
        span: s.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_static_call_expr(s: &StaticCallExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // `config::LIMIT`: an uppercase bare member parses like a fieldless
    // variant; a module's `const` takes precedence, as in the checker.
    //
    if s.is_bare_member()
        && let Some(value) = ctx.tables.module_constant(&s.class, &s.method)
    {
        return Ok(const_literal(value, s.span));
    }
    // A bare uppercase member on a class is a property, just like a
    // lowercase StaticField. Keep explicit `Member()` calls distinct.
    if s.is_bare_member() {
        let resolved = ctx.tables.static_call_class(&s.id, &s.class);
        let class = if resolved == "Self" {
            ctx.current_class.as_deref().unwrap_or(&resolved)
        } else {
            &resolved
        };
        if ctx.classes.map.contains_key(class)
            || ctx
                .tables
                .symbols
                .is_some_and(|symbols| symbols.lookup_class(class).is_some())
        {
            return lower_static_field_expr(
                &StaticFieldExpr {
                    id: s.id,
                    class: s.class.clone(),
                    field: s.method.clone(),
                    span: s.span,
                },
                ctx,
            );
        }
    }
    // `Enum::Variant(args)` construction parses like a static call.
    let class = if s.class == "Self" {
        ctx.current_class.as_deref().unwrap_or(&s.class)
    } else {
        &s.class
    };
    let variant_ty = enum_variant_value_type(ctx.enums, class, &s.method, false);
    let ty = variant_ty
        .or_else(|| ctx.classes.static_method_type(class, &s.method))
        // Checker authority: generic-enum construction, `Self::`,
        // module-qualified statics, constructors.
        .or_else(|| ctx.expr_type(&s.id))
        .ok_or_else(|| {
            unsupported(
                s.span,
                "static method not found (and no checker-recorded type)",
            )
        })?;
    let args = lower_value_args(&s.args, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::StaticCall {
            class: ctx.tables.static_call_class(&s.id, &s.class).into(),
            method: s.method.clone(),
            args,
        },
        ty: ty.into(),
        span: s.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_range_expr(r: &RangeExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let start = lower_expr(&r.start, ctx)?;
    let end = lower_expr(&r.end, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::Range {
            start: Box::new(start),
            end: Box::new(end),
        },
        ty: Type::Generic("Range".to_string().into(), vec![Type::I64]),
        span: r.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_select_expr(select: &SelectExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let mut cases = Vec::with_capacity(select.cases.len());
    for case in &select.cases {
        let kind = match &case.kind {
            SelectCaseKind::Recv {
                binding,
                channel,
                closed_aware,
            } => {
                let channel = lower_expr(channel, ctx)?;
                let elem_ty = builtin_types::unary_arg(&channel.ty, B::Channel)
                    .ok_or_else(|| unsupported(case.span, "select receive channel"))?
                    .clone();
                let binding_ty = if *closed_aware {
                    B::Option.apply(vec![elem_ty])
                } else {
                    elem_ty
                };
                ctx.push_scope();
                let binding = ctx.bind(binding.clone(), (binding_ty).to_source());
                let mut body = Vec::with_capacity(case.body.stmts.len());
                for stmt in &case.body.stmts {
                    body.push(lower_stmt(stmt, ctx)?);
                }
                ctx.pop_scope();
                cases.push(HirSelectCase {
                    kind: HirSelectCaseKind::Recv {
                        binding,
                        channel,
                        closed_aware: *closed_aware,
                    },
                    body,
                    span: case.span,
                });
                continue;
            }
            SelectCaseKind::Send { channel, value } => HirSelectCaseKind::Send {
                channel: lower_expr(channel, ctx)?,
                value: lower_expr(value, ctx)?,
            },
            SelectCaseKind::Timeout { millis } => HirSelectCaseKind::Timeout {
                millis: lower_expr(millis, ctx)?,
            },
            SelectCaseKind::Join { binding, task } => {
                let task = lower_expr(task, ctx)?;
                // What awaiting the operand YIELDS: `T` for `Task<T>`,
                // but `Result<T, Cancelled>` for `TaskResult<T>`. Taken
                // from the type checker's own definition so the HIR
                // binding cannot disagree with what the checker typed
                // the case body against.
                let binding_ty = await_output_type(&task.ty).ok_or_else(|| {
                    internal(
                        task.span,
                        "select join operand has no checked await output type".to_string(),
                    )
                })?;
                ctx.push_scope();
                let binding = ctx.bind(binding.clone(), (binding_ty).to_source());
                let mut body = Vec::with_capacity(case.body.stmts.len());
                for stmt in &case.body.stmts {
                    body.push(lower_stmt(stmt, ctx)?);
                }
                ctx.pop_scope();
                cases.push(HirSelectCase {
                    kind: HirSelectCaseKind::Join { binding, task },
                    body,
                    span: case.span,
                });
                continue;
            }
            SelectCaseKind::Default => HirSelectCaseKind::Default,
        };
        let body = lower_block(&case.body, ctx)?;
        cases.push(HirSelectCase {
            kind,
            body,
            span: case.span,
        });
    }
    Ok(HirExpr {
        kind: HirExprKind::Select { cases },
        ty: Type::Void,
        span: select.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_await_expr(a: &AwaitExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let inner = lower_expr(&a.expr, ctx)?;
    let resolved = builtin_types::resolve(&inner.ty)
        .filter(|resolved| resolved.args.len() == 1)
        .ok_or_else(|| unsupported(a.span, "await of a non-Task/Future value"))?;
    let ty = match resolved.id {
        B::Task | B::Future | B::JoinHandle => resolved.args[0].clone(),
        // `await task.result()` yields `Result<T, Cancelled>`.
        B::TaskResult => {
            B::Result.apply(vec![resolved.args[0].clone(), B::Cancelled.apply(vec![])])
        }
        _ => return Err(unsupported(a.span, "await of a non-Task/Future value")),
    };
    Ok(HirExpr {
        kind: HirExprKind::Await {
            inner: Box::new(inner),
        },
        ty,
        span: a.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_try_propagate_expr(
    inner: &Expr,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let inner = lower_expr(inner, ctx)?;
    let ty = builtin_types::resolve(&inner.ty)
        .filter(|resolved| matches!(resolved.id, B::Result | B::Option))
        .and_then(|resolved| resolved.args.first().cloned())
        .ok_or_else(|| unsupported(span, "`?` on a non-Result/Option value"))?;
    Ok(HirExpr {
        kind: HirExprKind::TryPropagate {
            inner: Box::new(inner),
        },
        ty,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_lambda_expr(l: &LambdaExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // The checker's inferred full `fn(...) -> ...` type fills in what
    // the AST cannot store: unannotated parameter types and, for
    // block-bodied lambdas, the inferred return type.
    let inferred = match ctx.tables.lambda_fn_type(&l.id) {
        Some(Type::Fn(params, ret) | Type::Closure(params, ret)) => {
            // Inferred types need the same import normalization as explicit
            // annotations before they become bindings in the lifted body.
            Some((
                params
                    .iter()
                    .map(|ty| ctx.normalize(ty))
                    .collect::<Vec<_>>(),
                ctx.normalize(ret),
            ))
        }
        _ => None,
    };
    let is_closure = matches!(ctx.tables.lambda_fn_type(&l.id), Some(Type::Closure(..)));
    // Resolve every capture in the ENCLOSING namespace first: after the
    // scope below, `n` means the lambda's own copy.
    let capture_sources: Vec<(String, String, Type, bool)> = ctx
        .tables
        .lambda_captures(&l.id)
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                ctx.hir_name(&c.name),
                ctx.normalize(&c.ty),
                ctx.tables.cell_declaration(&c.declaration_span),
            )
        })
        .collect();
    let mut params = Vec::with_capacity(l.params.len());
    let mut param_tys = Vec::with_capacity(l.params.len());
    ctx.push_scope();
    // A lambda body is lifted into a LirFunction of its own, so its
    // flat namespace is its own too and the shadow suffixes restart
    // here. Without this a lambda parameter could
    // arrive renamed after an unrelated binding of the same name in the
    // enclosing function — and the backend, which binds the lifted
    // parameters by their source names, would never bind it.
    let outer_binds = std::mem::take(&mut ctx.binds_seen);
    let outer_namespace_scope_base = ctx.namespace_scope_base;
    ctx.namespace_scope_base = ctx.scopes.len();
    // Bind the captures BEFORE anything else in the lambda's fresh
    // namespace. They are the first names it sees, so a body-local
    // rebinding of a captured name gets the shadow suffix and the two
    // stay distinct locals in the lifted function.
    let captures: Vec<HirCapture> = capture_sources
        .into_iter()
        .map(|(source_name, source, ty, cell)| HirCapture {
            name: ctx.bind_cell(source_name, ty.clone(), cell),
            source,
            ty: if cell {
                Type::Array(Box::new(ty)).into()
            } else {
                ty.into()
            },
        })
        .collect();
    for (i, p) in l.params.iter().enumerate() {
        let inferred_param = inferred
            .as_ref()
            .and_then(|(params, _)| params.get(i).cloned());
        let Some(ty) = p.ty.as_ref().map(|ty| ctx.normalize(ty)).or(inferred_param) else {
            ctx.pop_scope();
            ctx.binds_seen = outer_binds;
            ctx.namespace_scope_base = outer_namespace_scope_base;
            return Err(unsupported(p.span, "unannotated lambda parameter"));
        };
        let name = ctx.bind(p.name.clone(), ty.clone());
        param_tys.push(ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: false,
            span: p.span,
        });
    }
    let inferred_ret = inferred.map(|(_, ret)| ret);
    let (body, ret) = match &l.body {
        crate::parser::ast::LambdaBody::Expr(e) => {
            let value = match lower_expr(e, ctx) {
                Ok(value) => value,
                Err(err) => {
                    ctx.pop_scope();
                    ctx.binds_seen = outer_binds;
                    ctx.namespace_scope_base = outer_namespace_scope_base;
                    return Err(err);
                }
            };
            let ret = l
                .return_type
                .as_ref()
                .map(|ty| ctx.normalize(ty))
                .or(inferred_ret)
                .unwrap_or_else(|| (value.ty.clone()).to_source());
            let span = value.span;
            // A `void` body has no value to return: `|s| println(s)` is
            // a statement, and returning its non-existent value would
            // build a `return` against a signature with no result slot.
            //
            let body = if matches!(ret, Type::Void) {
                vec![HirStmt::Expr(value), HirStmt::Return { value: None, span }]
            } else {
                vec![HirStmt::Return {
                    value: Some(value),
                    span,
                }]
            };
            (body, ret)
        }
        crate::parser::ast::LambdaBody::Block(block) => {
            let Some(ret) = l
                .return_type
                .as_ref()
                .map(|ty| ctx.normalize(ty))
                .or(inferred_ret)
            else {
                ctx.pop_scope();
                ctx.binds_seen = outer_binds;
                ctx.namespace_scope_base = outer_namespace_scope_base;
                return Err(unsupported(
                    l.span,
                    "block-bodied lambda without a return type annotation",
                ));
            };
            let block = lower_block(block, ctx);
            match block {
                Ok(block) => (block, ret),
                Err(err) => {
                    ctx.pop_scope();
                    ctx.binds_seen = outer_binds;
                    ctx.namespace_scope_base = outer_namespace_scope_base;
                    return Err(err);
                }
            }
        }
    };
    ctx.pop_scope();
    ctx.binds_seen = outer_binds;
    ctx.namespace_scope_base = outer_namespace_scope_base;
    // A capture-free lambda that a `closure(...)` slot expects is a
    // closure too: the slot decides the calling convention, so the
    // lifted body still takes the (empty) environment.
    let ty = if is_closure {
        Type::Closure(param_tys, Box::new(ret))
    } else {
        Type::Fn(param_tys, Box::new(ret))
    };
    Ok(HirExpr {
        kind: HirExprKind::Lambda {
            id: l.id,
            params,
            captures,
            body,
        },
        ty: ty.into(),
        span: l.span,
    })
}

/// The value type of a checker-resolved unqualified variant construction.
/// Non-generic enums type as `Named(enum)`; a generic enum's type arguments
/// are not recorded in the resolution table, so they stay unsupported here.
fn enum_variant_construction_type(
    enums: &Enums,
    enum_name: &str,
    _args: &[HirExpr],
    span: Span,
) -> Result<Type, Diagnostic> {
    match enums.map.get(enum_name) {
        Some(info) if info.type_params.is_empty() => Ok(Type::Named(enum_name.to_string())),
        Some(_) => Err(unsupported(
            span,
            "generic enum construction (type arguments not in the resolution table)",
        )),
        None => Err(unsupported(span, "construction of an unknown enum")),
    }
}

/// The value type of constructing `Enum::Variant`. Non-generic enums only —
/// a generic variant's type arguments need inference from the expected type,
/// which the structural lowering does not thread through yet.
fn enum_variant_value_type(
    enums: &Enums,
    enum_name: &str,
    variant: &str,
    fieldless_only: bool,
) -> Option<Type> {
    let info = enums.map.get(enum_name)?;
    let payload = info.variants.get(variant)?;
    if !info.type_params.is_empty() || (fieldless_only && !payload.is_empty()) {
        return None;
    }
    Some(Type::Named(enum_name.to_string()))
}

/// Lower a `match` expression. Pattern bindings are typed from the scrutinee's
/// enum (type parameters substituted from its type arguments); the match's type
/// is the first arm type that is not `Never` (`Void` for block-bodied arms).
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_match(
    m: &crate::parser::ast::MatchExpr,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    use crate::parser::ast::{MatchBody, Pattern};

    let scrutinee = lower_expr(&m.scrutinee, ctx)?;

    // The scrutinee's enum context: its EnumInfo plus the substitution from the
    // enum's type parameters to the scrutinee's type arguments.
    let enum_context: Option<(String, EnumInfo, HashMap<String, Type>)> =
        match &scrutinee.ty.to_source() {
            Type::Named(name) => ctx
                .enums
                .map
                .get(name)
                .map(|info| (name.clone(), info.clone(), HashMap::new())),
            Type::Generic(name, args) => ctx.enums.map.get(name).map(|info| {
                let subst = info
                    .type_params
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                (name.clone(), info.clone(), subst)
            }),
            _ => None,
        };

    let mut arms = Vec::with_capacity(m.arms.len());
    for arm in &m.arms {
        ctx.push_scope();
        // The checker owns whether a bare spelling is a variant visible in
        // this unit. Structural inference is only for lowering without checker
        // tables; redoing it for checked input would turn catch-all bindings
        // into inaccessible imported variants.
        let infer_variant = ctx.tables.pattern_resolutions.is_none();
        let checked_pattern = ctx
            .tables
            .pattern_resolutions
            .and_then(|patterns| patterns.get(&arm.pattern.id()))
            .unwrap_or(&arm.pattern);
        let pattern = match checked_pattern {
            Pattern::Wildcard(_, _) => HirPattern::Wildcard,
            Pattern::LiteralBool(b, _, _) => HirPattern::LiteralBool(*b),
            Pattern::LiteralInt(n, _, _) => HirPattern::LiteralInt(*n),
            Pattern::Binding { name, .. } => {
                if let Some((enum_name, info, _)) = &enum_context
                    && infer_variant
                    && info.variants.get(name).is_some_and(Vec::is_empty)
                {
                    HirPattern::EnumVariant {
                        enum_name: enum_name.clone().into(),
                        variant: name.clone(),
                    }
                } else {
                    HirPattern::Binding {
                        name: ctx.bind(name.clone(), (scrutinee.ty.clone()).to_source()),
                        ty: scrutinee.ty.clone(),
                    }
                }
            }
            Pattern::EnumVariant { variant, .. } => {
                let Some((enum_name, _, _)) = &enum_context else {
                    ctx.pop_scope();
                    return Err(unsupported(
                        arm.span,
                        "enum pattern on a non-enum scrutinee",
                    ));
                };
                HirPattern::EnumVariant {
                    enum_name: enum_name.clone().into(),
                    variant: variant.clone(),
                }
            }
            Pattern::EnumVariantTuple {
                variant, bindings, ..
            } => bind_variant_tuple(&enum_context, variant, bindings, arm.span, ctx)?,
            Pattern::ClassDowncast {
                class_name,
                binding,
                ..
            } => {
                // Unqualified variant like `Some(x)` if it names a variant of the
                // scrutinee's enum; otherwise a real interface downcast.
                if infer_variant
                    && enum_context
                        .as_ref()
                        .is_some_and(|(_, info, _)| info.variants.contains_key(class_name))
                {
                    bind_variant_tuple(
                        &enum_context,
                        class_name,
                        std::slice::from_ref(binding),
                        arm.span,
                        ctx,
                    )?
                } else {
                    // Pattern spellings are unit-local, just like annotations.
                    // Both the runtime class test and its payload local must use
                    // the canonical identity used by checked field/method uses.
                    let binding_ty = ctx.tables.normalize(&Type::Named(class_name.clone()));
                    let Type::Named(canonical_class) = &binding_ty else {
                        return Err(unsupported(arm.span, "non-class downcast target"));
                    };
                    let binding = if binding == "_" {
                        binding.clone()
                    } else {
                        ctx.bind(binding.clone(), binding_ty.clone())
                    };
                    HirPattern::ClassDowncast {
                        class_name: canonical_class.clone().into(),
                        binding,
                        binding_ty: binding_ty.into(),
                    }
                }
            }
        };

        let guard = match &arm.guard {
            Some(guard) => Some(lower_expr(guard, ctx)?),
            None => None,
        };
        let (body, arm_ty) = match &arm.body {
            MatchBody::Expr(e) => {
                let value = lower_expr(e, ctx)?;
                let ty = value.ty.clone();
                (vec![HirStmt::Expr(value)], ty)
            }
            MatchBody::Block(block) => {
                let checked_exit = ctx
                    .tables
                    .arm_leaves
                    .and_then(|facts| facts.get(&block.id))
                    .copied();
                if ctx.tables.checked && checked_exit.is_none() {
                    return Err(missing_fact(block.span, "match arm exit"));
                }
                let ty = if checked_exit.unwrap_or_else(|| {
                    crate::semantic::type_checker::analysis::cached_arm_leaves(
                        block,
                        &mut ctx.arm_leaves,
                    )
                }) {
                    Type::Never
                } else {
                    Type::Void
                };
                (lower_block(block, ctx)?, ty)
            }
        };
        ctx.pop_scope();
        arms.push(HirMatchArm {
            pattern,
            guard,
            body,
            ty: arm_ty,
            span: arm.span,
        });
    }

    // `Never` arms (panic) coerce to the others; an all-diverging match stays
    // Never. Otherwise the checker's unified type is authoritative: arms of
    // distinct classes flowing into an interface take that interface.
    //
    let ty = arms
        .iter()
        .map(|a| &a.ty)
        .find(|t| **t != Type::Never)
        .cloned()
        .unwrap_or(Type::Never);
    let ty = match ctx.expr_type(&m.id) {
        Some(checked) if ty != Type::Never => checked.into(),
        _ => ty,
    };
    Ok(HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        },
        ty,
        span: m.span,
    })
}

/// Build an `EnumVariantTuple` pattern, binding each payload name to the
/// variant's (substituted) payload type. `_` bindings match without binding.
fn bind_variant_tuple(
    enum_context: &Option<(String, EnumInfo, HashMap<String, Type>)>,
    variant: &str,
    bindings: &[String],
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirPattern, Diagnostic> {
    let Some((enum_name, info, subst)) = enum_context else {
        return Err(unsupported(span, "enum pattern on a non-enum scrutinee"));
    };
    let Some(payload) = info.variants.get(variant) else {
        return Err(unsupported(span, "unknown enum variant in pattern"));
    };
    if payload.len() != bindings.len() {
        return Err(unsupported(span, "enum pattern arity mismatch"));
    }
    let mut typed = Vec::with_capacity(bindings.len());
    for (name, payload_ty) in bindings.iter().zip(payload) {
        let ty = subst_type(payload_ty, subst);
        let name = if name == "_" {
            name.clone()
        } else {
            ctx.bind(name.clone(), ty.clone())
        };
        typed.push((name, ty));
    }
    Ok(HirPattern::EnumVariantTuple {
        enum_name: enum_name.clone().into(),
        variant: variant.to_string(),
        bindings: typed
            .into_iter()
            .map(|(name, ty)| (name, ty.into()))
            .collect(),
    })
}
