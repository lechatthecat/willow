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
    lower_expr_inner(expr, checked_type, ctx)
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
fn lower_expr_inner(
    expr: &Expr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    match expr {
        Expr::Integer(n, span, _) => Ok(lit(
            HirExprKind::Int(*n as i64),
            ctx.normalize(checked_type.as_ref().unwrap_or(&Type::I64)),
            *span,
        )),
        Expr::Char(c, span, _) => Ok(lit(
            HirExprKind::Int(i64::from(u32::from(*c))),
            ctx.normalize(checked_type.as_ref().unwrap_or(&Type::Char)),
            *span,
        )),
        Expr::Float(f, span, _) => Ok(lit(
            HirExprKind::Float(*f),
            ctx.normalize(checked_type.as_ref().unwrap_or(&Type::F64)),
            *span,
        )),
        Expr::Bool(b, span, _) => Ok(lit(
            HirExprKind::Bool(*b),
            ctx.normalize(checked_type.as_ref().unwrap_or(&Type::Bool)),
            *span,
        )),
        Expr::String(s, span, _) => Ok(lit(
            HirExprKind::Str(s.clone()),
            ctx.normalize(checked_type.as_ref().unwrap_or(&Type::String)),
            *span,
        )),
        Expr::Var(name, span, id) => lower_var_expr(*id, name, *span, checked_type, ctx),
        Expr::Binary(_) | Expr::Unary(_) => lower_operator_tree(expr, ctx),
        Expr::Call(c) => lower_call_expr(c, checked_type, ctx),
        Expr::Print(inner, newline, span, _) => {
            lower_print_expr(inner, *newline, *span, checked_type, ctx)
        }
        Expr::ArrayLiteral(elements, span, _) => {
            lower_array_literal_expr(elements, *span, checked_type, ctx)
        }
        Expr::Index(array, index, span, _) => {
            lower_index_expr(array, index, *span, checked_type, ctx)
        }
        Expr::Ternary(t) => lower_ternary_expr(t, checked_type, ctx),
        Expr::New(n) => lower_new_expr(n, checked_type, ctx),
        Expr::FieldAccess(object, field, span, _) => {
            lower_field_access_expr(object, field, *span, checked_type, ctx)
        }
        Expr::MethodCall(m) => lower_method_call_expr(m, checked_type, ctx),
        Expr::ObjectLiteral(o) => lower_object_literal_expr(o, checked_type, ctx),
        Expr::StaticField(s) => lower_static_field_expr(s, checked_type, ctx),
        Expr::StaticCall(s) => lower_static_call_expr(s, checked_type, ctx),
        Expr::Range(r) => lower_range_expr(r, checked_type, ctx),
        Expr::Select(select) => lower_select_expr(select, checked_type, ctx),
        Expr::Await(a) => lower_await_expr(a, checked_type, ctx),
        Expr::TryPropagate(inner, span, _) => {
            lower_try_propagate_expr(inner, *span, checked_type, ctx)
        }
        Expr::Lambda(l) => lower_lambda_expr(l, checked_type, ctx),
        Expr::Match(m) => lower_match(m, checked_type, ctx),
    }
}

/// The allocated identity follows the normalized type, including import aliases.
fn allocation_type(
    name: &str,
    checked: Option<Type>,
    ctx: &LowerCtx,
) -> (
    crate::semantic::ids::TypeId,
    crate::semantic::ids::SemanticType,
) {
    let ty = ctx.result_type(checked, || Type::Named(name.to_owned()));
    let class = match &ty {
        Type::Named(class) => *class,
        _ => name.into(),
    };
    (class, ty)
}

fn normalized_static_class(
    class: crate::semantic::ids::TypeId,
    ctx: &LowerCtx,
) -> crate::semantic::ids::TypeId {
    let source = if class == crate::semantic::ids::TypeId::local("Self") {
        ctx.current_class
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| class.to_string())
    } else {
        class.to_string()
    };
    if let Type::Named(name) = &ctx.normalize(&Type::Named(source)) {
        name.as_str().into()
    } else {
        class
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
fn const_literal_kind(value: &crate::parser::ast::ConstValue) -> HirExprKind {
    use crate::parser::ast::ConstValue;
    match value {
        ConstValue::Integer(n) => HirExprKind::Int(*n as i64),
        ConstValue::Char(c) => HirExprKind::Int(i64::from(u32::from(*c))),
        ConstValue::Float(f) => HirExprKind::Float(*f),
        ConstValue::Bool(b) => HirExprKind::Bool(*b),
        ConstValue::String(s) => HirExprKind::Str(s.clone()),
    }
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
    id: ExprId,
    name: &str,
    span: Span,
    checked_type: Option<Type>,
    ctx: &LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    if let Some(binding) = ctx.lookup(name) {
        let ty = ctx.normalize(checked_type.as_ref().unwrap_or(&binding.ty));
        return Ok(binding_read(binding, ty.into(), span));
    }

    let (kind, ty) = if let Some(enum_name) = ctx.tables.enum_variant_resolution(&id) {
        // A bare fieldless variant parses as a variable.
        (
            HirExprKind::StaticField {
                class: normalized_static_class(enum_name.as_str().into(), ctx),
                field: name.to_string(),
            },
            checked_type.unwrap_or_else(|| Type::Named(enum_name.to_string())),
        )
    } else if let Some(value) = ctx.tables.constant(name) {
        (
            const_literal_kind(value),
            checked_type.unwrap_or_else(|| value.ty()),
        )
    } else if (ctx.fn_returns.contains_key(name)
        || ctx
            .tables
            .symbols
            .is_some_and(|symbols| symbols.lookup_func(name).is_some()))
        && let Some(ty @ (Type::Fn(..) | Type::Closure(..))) = checked_type
    {
        // An imported function value may be known only to the checker.
        (HirExprKind::FnRef(name.into()), ty)
    } else {
        return Err(internal(
            span,
            format!("unbound variable `{name}` reached HIR lowering"),
        ));
    };
    Ok(lit(kind, ctx.normalize(&ty), span))
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
        ShortCircuitRhs(&'a BinaryExpr),
        ResumeRegistrations,
        Unary(&'a UnaryExpr),
    }
    let mut work = vec![Step::Enter(expr)];
    let mut values: Vec<HirExpr> = Vec::new();
    while let Some(step) = work.pop() {
        let expr = match step {
            Step::Enter(expr) => expr,
            Step::ShortCircuitRhs(binary) => {
                let lhs = values.last().expect("lowered short-circuit condition");
                if !defer_stability::rhs_reachable(&binary.op, lhs) {
                    ctx.deferred_callees.skipped = true;
                    work.push(Step::ResumeRegistrations);
                }
                &binary.rhs
            }
            Step::ResumeRegistrations => {
                ctx.deferred_callees.skipped = false;
                continue;
            }
            Step::Binary(binary) => {
                let rhs = values.pop().expect("lowered right operand");
                let lhs = values.pop().expect("lowered left operand");
                let ty = required_expr_type(binary.id, binary.span, ctx)?
                    .unwrap_or_else(|| binary_result_type(&binary.op, &lhs.ty.to_source()));
                values.push(HirExpr {
                    kind: HirExprKind::Binary {
                        op: binary.op.clone(),
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    ty: ctx.normalize(&ty).into(),
                    span: binary.span,
                });
                continue;
            }
            Step::Unary(unary) => {
                let operand = values.pop().expect("lowered unary operand");
                let ty = required_expr_type(unary.id, unary.span, ctx)?.unwrap_or_else(|| {
                    match unary.op {
                        UnaryOp::Neg => operand.ty.to_source(),
                        // `!` is bitwise on `i64` and logical on `bool`.
                        UnaryOp::Not if operand.ty.integer_layout().is_some() => {
                            operand.ty.to_source()
                        }
                        UnaryOp::Not => Type::Bool,
                    }
                });
                values.push(HirExpr {
                    kind: HirExprKind::Unary {
                        op: unary.op.clone(),
                        operand: Box::new(operand),
                    },
                    ty: ctx.normalize(&ty).into(),
                    span: unary.span,
                });
                continue;
            }
        };
        match expr {
            Expr::Binary(binary) => {
                work.push(Step::Binary(binary));
                if matches!(binary.op, BinOp::And | BinOp::Or) && !ctx.deferred_callees.skipped {
                    work.push(Step::ShortCircuitRhs(binary));
                } else {
                    work.push(Step::Enter(&binary.rhs));
                }
                work.push(Step::Enter(&binary.lhs));
            }
            Expr::Unary(unary) => {
                if unary.op == UnaryOp::Neg
                    && let Expr::Integer(n, _, _) = &unary.expr
                {
                    let ty = required_expr_type(unary.id, unary.span, ctx)?.unwrap_or(Type::I64);
                    values.push(lit(
                        HirExprKind::Int((-*n) as i64),
                        if work.is_empty() {
                            ctx.normalize(&ty)
                        } else {
                            ty
                        },
                        unary.span,
                    ));
                    continue;
                }
                work.push(Step::Unary(unary));
                work.push(Step::Enter(&unary.expr));
            }
            other => values.push(lower_expr(other, ctx)?),
        }
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
fn lower_call_expr(
    c: &CallExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    if matches!(c.callee.as_str(), "forall" | "exists") {
        if c.args.len() != 3 || !c.type_args.is_empty() {
            return Err(unsupported(c.span, "malformed quantified expression"));
        }
        let Expr::Lambda(predicate) = &c.args[2].expr else {
            return Err(unsupported(c.span, "quantifier predicate binder"));
        };
        let crate::parser::ast::LambdaBody::Expr(body) = &predicate.body else {
            return Err(unsupported(c.span, "block-bodied quantifier predicate"));
        };
        if predicate.params.len() != 1 {
            return Err(unsupported(c.span, "quantifier binder arity"));
        }
        // Bounds are lowered in the enclosing scope and evaluated exactly once.
        let lo = lower_expr(&c.args[0].expr, ctx)?;
        let hi = lower_expr(&c.args[1].expr, ctx)?;
        ctx.push_scope();
        let index = ctx.bind(predicate.params[0].name.clone(), Type::I64);
        let limit = ctx.bind(format!("$quantifier_limit_{}", c.id), Type::I64);
        let result = ctx.bind(format!("$quantifier_result_{}", c.id), Type::Bool);
        let value = lower_expr(body, ctx);
        ctx.pop_scope();
        let value = value?;
        let span = c.span;
        let universal = c.callee == "forall";
        let variable = |name: &str, ty: Type| lit(HirExprKind::Var(name.to_owned()), ty, span);
        let condition = lit(
            HirExprKind::Binary {
                op: BinOp::Lt,
                lhs: Box::new(variable(&index, Type::I64)),
                rhs: Box::new(variable(&limit, Type::I64)),
            },
            Type::Bool,
            span,
        );
        let decisive = if universal {
            lit(
                HirExprKind::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(value),
                },
                Type::Bool,
                span,
            )
        } else {
            value
        };
        let increment = lit(
            HirExprKind::Binary {
                op: BinOp::Add,
                lhs: Box::new(variable(&index, Type::I64)),
                rhs: Box::new(lit(HirExprKind::Int(1), Type::I64, span)),
            },
            Type::I64,
            span,
        );
        // index < limit <= i64::MAX establishes that this increment cannot overflow.
        let statements = vec![
            HirStmt::Let {
                name: index.clone(),
                mutable: true,
                ty: Type::I64,
                value: lo,
                span,
            },
            HirStmt::Let {
                name: limit,
                mutable: false,
                ty: Type::I64,
                value: hi,
                span,
            },
            HirStmt::Let {
                name: result.clone(),
                mutable: true,
                ty: Type::Bool,
                value: lit(HirExprKind::Bool(universal), Type::Bool, span),
                span,
            },
            HirStmt::While {
                cond: condition,
                body: vec![
                    HirStmt::conditional(
                        decisive,
                        vec![
                            HirStmt::Assign {
                                name: result.clone(),
                                value: lit(HirExprKind::Bool(!universal), Type::Bool, span),
                                span,
                            },
                            HirStmt::Break { span },
                        ],
                        None,
                        span,
                    ),
                    HirStmt::Assign {
                        name: index,
                        value: increment,
                        span,
                    },
                ],
                span,
            },
            HirStmt::Expr(variable(&result, Type::Bool)),
        ];
        return Ok(lit(
            HirExprKind::Match {
                scrutinee: Box::new(lit(HirExprKind::Bool(true), Type::Bool, span)),
                arms: vec![HirMatchArm {
                    pattern: HirPattern::Wildcard,
                    guard: Vec::new(),
                    body: statements,
                    ty: Type::Bool,
                    span,
                }],
            },
            ctx.normalize(checked_type.as_ref().unwrap_or(&Type::Bool)),
            span,
        ));
    }
    let args = lower_value_args(&c.args, ctx)?;
    let binding = ctx.lookup(&c.callee);
    if let Some(binding) = binding.filter(|b| b.storage == BindingStorage::Cell) {
        let (Type::Fn(_, ret) | Type::Closure(_, ret)) = &binding.ty else {
            return Err(internal(c.span, "non-callable capture cell".to_string()));
        };
        return Ok(HirExpr {
            kind: HirExprKind::Invoke {
                name: c.callee.clone().into(),
                callee: Box::new(binding_read(binding, (&binding.ty).into(), c.span)),
                args,
            },
            ty: ctx.normalize(checked_type.as_ref().unwrap_or(ret)).into(),
            span: c.span,
        });
    }
    // An unqualified enum-variant construction (`Ok(42)` in an
    // expected-enum position) parses as a call; the checker records
    // which enum it resolved to.
    if let Some(enum_name) = ctx.tables.enum_variant_resolution(&c.id) {
        // Prefer the checker's recorded type (it carries generic type
        // arguments, e.g. `Result<i64, String>` for `Ok(42)`).
        let ty = match checked_type {
            Some(ty) => ty,
            None => enum_variant_construction_type(ctx.enums, enum_name, &args, c.span)?,
        };
        return Ok(HirExpr {
            kind: HirExprKind::StaticCall {
                class: normalized_static_class(enum_name.as_str().into(), ctx),
                method: c.callee.clone(),
                args,
            },
            ty: ctx.normalize(&ty).into(),
            span: c.span,
        });
    }
    // A local fn-typed variable shadows a free function; its call is an
    // indirect call typed by the variable's `fn(..) -> R`, and it is
    // named by the binding's HIR name.
    let (callee, binding, ty) = if let Some((b, ret)) = binding.and_then(|b| match &b.ty {
        Type::Fn(_, ret) | Type::Closure(_, ret) => Some((b, ret.as_ref())),
        _ => None,
    }) {
        (
            b.hir_name.as_str().into(),
            HirCallBinding::Local {
                may_change: b.storage == BindingStorage::MayChange,
            },
            ctx.normalize(checked_type.as_ref().unwrap_or(ret)),
        )
    } else {
        // Reserve the free name before lowering a later shadowing binding.
        if !ctx.binds_seen.contains_key(&c.callee) {
            ctx.binds_seen.insert(c.callee.clone(), 1);
        }
        // Imported functions may exist only in the checker's tables.
        let ty = checked_type
            .as_ref()
            .or_else(|| ctx.fn_returns.get(&c.callee))
            .ok_or_else(|| {
                internal(
                    c.span,
                    format!(
                        "call to unknown function `{}` reached HIR lowering",
                        c.callee
                    ),
                )
            })?;
        (
            c.callee.as_str().into(),
            HirCallBinding::Function,
            ctx.normalize(ty),
        )
    };
    Ok(HirExpr {
        kind: HirExprKind::Call {
            callee,
            binding,
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
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let value = lower_expr(inner, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::Print {
            value: Box::new(value),
            newline,
        },
        ty: ctx.result_type(checked_type, || Type::Void),
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
    elements: &[Expr],
    span: Span,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let mut lowered = Vec::with_capacity(elements.len());
    for element in elements {
        lowered.push(lower_expr(element, ctx)?);
    }
    // The checker owns contextual typing for every literal, not only empty
    // ones. Inferring again from element 0 loses interface storage/boxing.
    // The first-element fallback supports callers lowering unchecked syntax.
    let ty = match checked_type {
        Some(ty) if !lowered.is_empty() || matches!(ty, Type::Array(_)) => ty,
        _ => {
            let first = lowered.first().ok_or_else(|| {
                unsupported(span, "empty array literal with no recorded element type")
            })?;
            Type::Array(Box::new(first.ty.to_source()))
        }
    };
    Ok(HirExpr {
        kind: HirExprKind::Array { elements: lowered },
        ty: ctx.normalize(&ty).into(),
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
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let array = lower_expr(array, ctx)?;
    let index = lower_expr(index, ctx)?;
    // A `FrozenArray<T>` is the same runtime handle as the `Array<T>` it
    // was frozen from, and `arr[i]` reads it the same way, so both lower
    // to one `Index` node. `Range<i64>` also spells a
    // read this way but is not a handle at all, so it stays unsupported.
    let element = match &array.ty {
        Type::Array(element) => element.as_ref(),
        ty => match builtin_types::unary_arg(ty, B::FrozenArray) {
            Some(element) => element,
            None => return Err(unsupported(span, "index of a non-array value")),
        },
    };
    let ty = ctx.result_type(checked_type, || element.to_source());
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
fn lower_ternary_expr(
    t: &TernaryExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let condition = lower_expr(&t.condition, ctx)?;
    let enclosing_skipped = ctx.deferred_callees.skipped;
    ctx.deferred_callees.skipped |= !defer_stability::branch_reachable(&condition, true);
    let then_expr = lower_expr(&t.then_expr, ctx)?;
    ctx.deferred_callees.skipped =
        enclosing_skipped || !defer_stability::branch_reachable(&condition, false);
    let else_expr = lower_expr(&t.else_expr, ctx)?;
    ctx.deferred_callees.skipped = enclosing_skipped;
    // A diverging arm contributes no value to the result. If both diverge,
    // the result remains Never. Otherwise the checker's type is
    // authoritative: branches of distinct classes flowing into an interface
    // take that interface.
    let ty = ctx.result_type(checked_type, || {
        if then_expr.ty == Type::Never {
            else_expr.ty.to_source()
        } else {
            then_expr.ty.to_source()
        }
    });
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
fn lower_new_expr(
    n: &NewExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let args = lower_value_args(&n.args, ctx)?;
    let (class, ty) = allocation_type(&n.class_name, checked_type, ctx);
    Ok(HirExpr {
        kind: HirExprKind::New { class, args },
        ty,
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
    object: &Expr,
    field: &str,
    span: Span,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let object = lower_expr(object, ctx)?;
    let ty = checked_type
        .as_ref()
        .or_else(|| {
            class_name_of(&object.ty).and_then(|class| ctx.classes.field_type(&class, field))
        })
        .ok_or_else(|| unsupported(span, "field not found on receiver"))?;
    Ok(HirExpr {
        kind: HirExprKind::FieldAccess {
            object: Box::new(object),
            field: field.to_string(),
        },
        ty: ctx.normalize(ty).into(),
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
fn lower_method_call_expr(
    m: &MethodCallExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    // `super.method(args)`: the receiver is `self`, and the checker typed the
    // `super` receiver as the base class whose implementation is called.
    if let Expr::Var(name, span, id) = &m.object
        && name == "super"
        && ctx.lookup("super").is_none()
    {
        let base = ctx.expr_type(id);
        let (Some(Type::Named(base)), Some(ty)) = (&base, checked_type) else {
            return Err(unsupported(m.span, "unresolved `super` method call"));
        };
        let Some(binding) = ctx.lookup("self") else {
            return Err(unsupported(*span, "`super` outside an instance method"));
        };
        let object = HirExpr {
            kind: HirExprKind::Var(binding.hir_name.to_string()),
            ty: (&binding.ty).into(),
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
                builtin: None,
                super_class: Some(base.as_str().into()),
            },
            ty: ctx.normalize(&ty).into(),
            span: m.span,
        });
    }
    let object = lower_expr(&m.object, ctx)?;
    // Partial lowering also serves standalone callers that have no checker facts.
    let standalone = ctx
        .tables
        .builtin_calls
        .is_none()
        .then(|| m.method.kind().lookup(&object.ty));
    let builtin = ctx
        .tables
        .builtin_calls
        .and_then(|calls| calls.checked(&m.id))
        .or_else(|| {
            standalone
                .as_ref()?
                .identity()
                .filter(|(_, arity)| *arity == m.args.len())
                .map(|(id, _)| id)
        });
    use crate::semantic::intrinsics::{IntrinsicReturn, MethodLookup};
    let result = checked_type
        .map(IntrinsicReturn::Fixed)
        .or_else(|| match standalone? {
            MethodLookup::Intrinsic(sig) if sig.arity == m.args.len() => Some(match sig.ret {
                IntrinsicReturn::Fixed(ty) => IntrinsicReturn::Fixed(ty.to_source()),
                IntrinsicReturn::SameAsArg(i) => IntrinsicReturn::SameAsArg(i),
            }),
            _ => None,
        })
        .or_else(|| {
            class_name_of(&object.ty)
                .and_then(|class| ctx.classes.method_type(&class, &m.method))
                .map(IntrinsicReturn::Fixed)
        })
        .ok_or_else(|| unsupported(m.span, "method not found on receiver"))?;
    let args = lower_value_args(&m.args, ctx)?;
    let ty = match result {
        IntrinsicReturn::Fixed(ty) => ty,
        IntrinsicReturn::SameAsArg(i) => args[i].ty.to_source(),
    };
    let owner = ctx
        .tables
        .static_call_classes
        .and_then(|owners| owners.get(&m.id))
        .cloned();
    let forward_length = owner.as_ref().is_some_and(|owner| {
        ctx.tables
            .symbols
            .and_then(|symbols| symbols.lookup_class(&owner.to_string()))
            .and_then(|class| class.methods.get(m.method.as_str()))
            .is_some_and(|method| {
                method.array_body_optimization == crate::parser::ast::ArrayBodyOptimization::Length
            })
    });
    Ok(HirExpr {
        kind: HirExprKind::MethodCall {
            object: Box::new(object),
            builtin: if forward_length {
                Some(BuiltinMethod::Intrinsic(Intrinsic::ArrayLen))
            } else {
                builtin
            },
            method: if forward_length {
                crate::parser::ast::MethodSpelling::fixed(
                    crate::semantic::intrinsics::MethodName::AbiLen,
                )
            } else {
                m.method.clone()
            },
            args,
            super_class: if forward_length { None } else { owner },
        },
        ty: ctx.normalize(&ty).into(),
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
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let mut fields = Vec::with_capacity(o.fields.len());
    for f in &o.fields {
        fields.push((f.name.clone(), lower_expr(&f.value, ctx)?));
    }
    let (class, ty) = allocation_type(&o.class, checked_type, ctx);
    Ok(HirExpr {
        kind: HirExprKind::ObjectLiteral { class, fields },
        ty,
        span: o.span,
    })
}

#[inline(never)]
fn lower_static_field_expr(
    s: &StaticFieldExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    // `config::LIMIT` names a module's `const` before any class,
    // matching the checker's precedence.
    if let Some(value) = ctx.tables.module_constant(&s.class, &s.field) {
        return Ok(lit(
            const_literal_kind(value),
            ctx.normalize(&checked_type.unwrap_or_else(|| value.ty())),
            s.span,
        ));
    }
    // `Enum::Variant` (fieldless) parses like a static property read.
    let resolved = ctx.tables.static_call_class(&s.id, &s.class);
    let class = if resolved == "Self" {
        ctx.current_class.as_deref().unwrap_or(&resolved)
    } else {
        &resolved
    };
    let ty = checked_type
        .or_else(|| enum_variant_value_type(ctx.enums, class, &s.field, true))
        .or_else(|| ctx.classes.static_field_type(class, &s.field))
        .ok_or_else(|| unsupported(s.span, "static property not found"))?;
    Ok(HirExpr {
        kind: HirExprKind::StaticField {
            class: normalized_static_class(class.into(), ctx),
            field: s.field.clone(),
        },
        ty: ctx.normalize(&ty).into(),
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
fn lower_static_call_expr(
    s: &StaticCallExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    // `config::LIMIT`: an uppercase bare member parses like a fieldless
    // variant; a module's `const` takes precedence, as in the checker.
    //
    if s.is_bare_member()
        && let Some(value) = ctx.tables.module_constant(&s.class, &s.method)
    {
        return Ok(lit(
            const_literal_kind(value),
            ctx.normalize(&checked_type.unwrap_or_else(|| value.ty())),
            s.span,
        ));
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
                checked_type,
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
    let ty = checked_type
        .or_else(|| enum_variant_value_type(ctx.enums, class, &s.method, false))
        .or_else(|| ctx.classes.static_method_type(class, &s.method))
        .ok_or_else(|| {
            unsupported(
                s.span,
                "static method not found (and no checker-recorded type)",
            )
        })?;
    let args = lower_value_args(&s.args, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::StaticCall {
            class: normalized_static_class(
                ctx.tables.static_call_class(&s.id, &s.class).into(),
                ctx,
            ),
            method: s.method.clone(),
            args,
        },
        ty: ctx.normalize(&ty).into(),
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
fn lower_range_expr(
    r: &RangeExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let start = lower_expr(&r.start, ctx)?;
    let end = lower_expr(&r.end, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::Range {
            start: Box::new(start),
            end: Box::new(end),
        },
        ty: ctx.result_type(checked_type, || B::Range.apply(vec![Type::I64])),
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
fn lower_select_expr(
    select: &SelectExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
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
                    .to_source();
                let binding_ty = if *closed_aware {
                    B::Option.apply(vec![elem_ty])
                } else {
                    elem_ty
                };
                ctx.push_scope();
                let binding = ctx.bind(binding.clone(), binding_ty);
                let enclosing_skipped = ctx.deferred_callees.skipped;
                let mut body = Vec::with_capacity(case.body.stmts.len());
                for stmt in &case.body.stmts {
                    body.push(lower_stmt(stmt, ctx)?);
                }
                ctx.deferred_callees.skipped = enclosing_skipped;
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
                let enclosing_skipped = ctx.deferred_callees.skipped;
                let mut body = Vec::with_capacity(case.body.stmts.len());
                for stmt in &case.body.stmts {
                    body.push(lower_stmt(stmt, ctx)?);
                }
                ctx.deferred_callees.skipped = enclosing_skipped;
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
        let body = lower_block(&case.body, ctx, true)?;
        cases.push(HirSelectCase {
            kind,
            body,
            span: case.span,
        });
    }
    Ok(HirExpr {
        kind: HirExprKind::Select { cases },
        ty: ctx.result_type(checked_type, || Type::Void),
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
fn lower_await_expr(
    a: &AwaitExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let inner = lower_expr(&a.expr, ctx)?;
    let resolved = builtin_types::resolve(&inner.ty)
        .filter(|resolved| resolved.args.len() == 1)
        .ok_or_else(|| unsupported(a.span, "await of a non-Task/Future value"))?;
    let ty = match resolved.id {
        B::Task | B::Future | B::JoinHandle => {
            ctx.result_type(checked_type, || resolved.args[0].to_source())
        }
        B::TaskResult => ctx.result_type(checked_type, || {
            B::Result.apply(vec![
                resolved.args[0].to_source(),
                B::Cancelled.apply(vec![]),
            ])
        }),
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
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let inner = lower_expr(inner, ctx)?;
    let payload = builtin_types::resolve(&inner.ty)
        .filter(|resolved| matches!(resolved.id, B::Result | B::Option))
        .and_then(|resolved| resolved.args.first())
        .ok_or_else(|| unsupported(span, "`?` on a non-Result/Option value"))?;
    let ty = ctx.result_type(checked_type, || payload.to_source());
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
fn lower_lambda_expr(
    l: &LambdaExpr,
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    // The checker's inferred full `fn(...) -> ...` type fills in what
    // the AST cannot store: unannotated parameter types and, for
    // block-bodied lambdas, the inferred return type.
    let inferred = match &checked_type {
        Some(Type::Fn(params, ret) | Type::Closure(params, ret)) => Some((params, ret.as_ref())),
        _ => None,
    };
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

    ctx.push_scope();
    // A lambda body is lifted into a LirFunction of its own, so its
    // flat namespace is its own too and the shadow suffixes restart
    // here. Without this a lambda parameter could
    // arrive renamed after an unrelated binding of the same name in the
    // enclosing function — and the backend, which binds the lifted
    // parameters by their source names, would never bind it.
    let outer_binds = std::mem::take(&mut ctx.binds_seen);
    let outer_callees = std::mem::take(&mut ctx.deferred_callees);
    let outer_namespace_scope_base = ctx.namespace_scope_base;
    ctx.namespace_scope_base = ctx.scopes.len();
    // Bind the captures BEFORE anything else in the lambda's fresh
    // namespace. They are the first names it sees, so a body-local
    // rebinding of a captured name gets the shadow suffix and the two
    // stay distinct locals in the lifted function.
    let captures: Vec<HirCapture> = capture_sources
        .into_iter()
        .map(|(source_name, source, ty, cell)| HirCapture {
            name: ctx.bind_with_storage(
                source_name,
                ty.clone(),
                if cell {
                    BindingStorage::Cell
                } else {
                    BindingStorage::Stable
                },
            ),
            source,
            ty: if cell {
                Type::Array(Box::new(ty)).into()
            } else {
                ty.into()
            },
        })
        .collect();
    for (i, p) in l.params.iter().enumerate() {
        let Some(ty) =
            p.ty.as_ref()
                .or_else(|| inferred.and_then(|(params, _)| params.get(i)))
        else {
            ctx.pop_scope();
            ctx.deferred_callees = outer_callees;
            ctx.binds_seen = outer_binds;
            ctx.namespace_scope_base = outer_namespace_scope_base;
            return Err(unsupported(p.span, "unannotated lambda parameter"));
        };
        let ty = ctx.normalize(ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: false,
            span: p.span,
        });
    }
    let declared_ret = l
        .return_type
        .as_ref()
        .or_else(|| inferred.map(|(_, ret)| ret));
    let (mut body, ret) = match &l.body {
        crate::parser::ast::LambdaBody::Expr(e) => {
            let value = match lower_expr(e, ctx) {
                Ok(value) => value,
                Err(err) => {
                    ctx.pop_scope();
                    ctx.deferred_callees = outer_callees;
                    ctx.binds_seen = outer_binds;
                    ctx.namespace_scope_base = outer_namespace_scope_base;
                    return Err(err);
                }
            };
            let ret = declared_ret
                .map(|ty| ctx.normalize(ty))
                .unwrap_or_else(|| value.ty.to_source());
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
            let Some(ret) = declared_ret.map(|ty| ctx.normalize(ty)) else {
                ctx.pop_scope();
                ctx.deferred_callees = outer_callees;
                ctx.binds_seen = outer_binds;
                ctx.namespace_scope_base = outer_namespace_scope_base;
                return Err(unsupported(
                    l.span,
                    "block-bodied lambda without a return type annotation",
                ));
            };
            let block = lower_block(block, ctx, true);
            match block {
                Ok(block) => (block, ret),
                Err(err) => {
                    ctx.pop_scope();
                    ctx.deferred_callees = outer_callees;
                    ctx.binds_seen = outer_binds;
                    ctx.namespace_scope_base = outer_namespace_scope_base;
                    return Err(err);
                }
            }
        }
    };
    ctx.deferred_callees
        .resolve(&mut body, None, ctx.tables.symbols);
    ctx.pop_scope();
    ctx.deferred_callees = outer_callees;
    ctx.binds_seen = outer_binds;
    ctx.namespace_scope_base = outer_namespace_scope_base;
    // The checker's kind (shareable or task-local) only constrains where
    // the value may flow; every function value has the same representation.
    let ty = ctx.result_type(checked_type, || {
        Type::Fn(
            params.iter().map(|p| p.ty.to_source()).collect(),
            Box::new(ret),
        )
    });
    Ok(HirExpr {
        kind: HirExprKind::Lambda {
            id: l.id,
            params,
            captures,
            body,
        },
        ty,
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
    checked_type: Option<Type>,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    use crate::parser::ast::{Guard, MatchBody};

    let scrutinee = lower_expr(&m.scrutinee, ctx)?;
    let scrutinee_ty = scrutinee.ty.to_source();
    // The checker owns whether a bare spelling is a variant visible in this
    // unit. Structural inference is only for lowering without checker tables;
    // redoing it for checked input would turn catch-all bindings into
    // inaccessible imported variants.
    let infer_variant = ctx.tables.pattern_resolutions.is_none();

    let mut arms = Vec::with_capacity(m.arms.len());
    // Nested payload patterns of the step being lowered, reused across arms.
    let mut nested = Vec::new();
    for arm in &m.arms {
        ctx.push_scope();
        let checked_pattern = ctx
            .tables
            .pattern_resolutions
            .and_then(|patterns| patterns.get(&arm.pattern.id()))
            .unwrap_or(&arm.pattern);
        let pattern = lower_flat_pattern(
            checked_pattern,
            &scrutinee_ty,
            infer_variant,
            arm.span,
            ctx,
            &mut nested,
        )?;
        // Nested tests run before the source guard chain: the chain may use
        // their bindings.
        let mut guard = Vec::with_capacity(arm.guard.len());
        drain_nested_patterns(&mut nested, infer_variant, ctx, &mut guard)?;
        for step in &arm.guard {
            match step {
                Guard::If(cond) => {
                    let cond = lower_expr(cond, ctx)?;
                    guard.push(HirGuard::If(cond));
                }
                Guard::Let(step) => {
                    let value = lower_expr(&step.scrutinee, ctx)?;
                    let checked = ctx
                        .tables
                        .pattern_resolutions
                        .and_then(|patterns| patterns.get(&step.pattern.id()))
                        .unwrap_or(&step.pattern);
                    let pattern = lower_flat_pattern(
                        checked,
                        &value.ty.to_source(),
                        infer_variant,
                        step.span,
                        ctx,
                        &mut nested,
                    )?;
                    guard.push(HirGuard::Let {
                        scrutinee: value,
                        pattern,
                    });
                    drain_nested_patterns(&mut nested, infer_variant, ctx, &mut guard)?;
                }
            }
        }
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
                (lower_block(block, ctx, true)?, ty)
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
    let ty = ctx.result_type(checked_type, || {
        arms.iter()
            .find(|arm| arm.ty != Type::Never)
            .map_or(Type::Never, |arm| arm.ty.to_source())
    });
    Ok(HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        },
        ty,
        span: m.span,
    })
}

/// A nested payload pattern whose test still has to be emitted: the payload
/// is bound to the synthetic local `local` of type `ty`.
struct PendingPattern<'p> {
    local: String,
    ty: Type,
    pattern: &'p crate::parser::ast::Pattern,
    span: Span,
}

/// The payload positions of an enum pattern: nested patterns, or the single
/// name of a bare `V(x)` that parsed as a class downcast.
#[derive(Clone, Copy)]
enum PayloadFields<'p> {
    Patterns(&'p [crate::parser::ast::Pattern]),
    Name(&'p String),
}

/// Emit one `let` step per pending nested pattern, in FIFO order. Each step
/// may queue its own payload patterns, so every source pattern node is
/// visited exactly once.
fn drain_nested_patterns<'p>(
    nested: &mut Vec<PendingPattern<'p>>,
    infer_variant: bool,
    ctx: &mut LowerCtx,
    steps: &mut Vec<HirGuard>,
) -> Result<(), Diagnostic> {
    let mut next = 0;
    while next < nested.len() {
        let pending = &mut nested[next];
        let local = std::mem::take(&mut pending.local);
        let ty = std::mem::replace(&mut pending.ty, Type::Void);
        let (pattern, span) = (pending.pattern, pending.span);
        next += 1;
        let flat = lower_flat_pattern(pattern, &ty, infer_variant, span, ctx, nested)?;
        steps.push(HirGuard::Let {
            scrutinee: HirExpr {
                kind: HirExprKind::Var(local),
                ty: ty.into(),
                span,
            },
            pattern: flat,
        });
    }
    nested.clear();
    Ok(())
}

/// The enum named by `ty` and the payload types of its `variant` with the
/// enum's type parameters substituted (`None` when the enum lacks the
/// variant). `None` for a non-enum type.
fn enum_variant_payload(
    ctx: &LowerCtx,
    ty: &Type,
    variant: &str,
) -> Option<(String, Option<Vec<Type>>)> {
    let (name, args) = match ty {
        Type::Named(name) => (name, &[][..]),
        Type::Generic(name, args) => (name, args.as_slice()),
        _ => return None,
    };
    let info = ctx.enums.map.get(name)?;
    let payload = info.variants.get(variant).map(|payload| {
        if args.is_empty() {
            return payload.clone();
        }
        let subst = info
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        payload.iter().map(|ty| subst_type(ty, &subst)).collect()
    });
    Some((name.clone(), payload))
}

/// Whether `name` is a fieldless variant of the enum `ty` (structural
/// inference for lowering without checker tables).
fn infers_fieldless_variant(ctx: &LowerCtx, ty: &Type, name: &str) -> bool {
    let name_of = match ty {
        Type::Named(enum_name) | Type::Generic(enum_name, _) => enum_name,
        _ => return false,
    };
    ctx.enums
        .map
        .get(name_of)
        .and_then(|info| info.variants.get(name))
        .is_some_and(Vec::is_empty)
}

/// Lower one pattern node over a value of type `ty` to a flat HIR pattern.
/// Payload positions that are neither a binding nor `_` bind a synthetic local
/// and are queued on `nested` for a later `let` step.
fn lower_flat_pattern<'p>(
    pattern: &'p crate::parser::ast::Pattern,
    ty: &Type,
    infer_variant: bool,
    span: Span,
    ctx: &mut LowerCtx,
    nested: &mut Vec<PendingPattern<'p>>,
) -> Result<HirPattern, Diagnostic> {
    use crate::parser::ast::Pattern;
    Ok(match pattern {
        Pattern::Wildcard(_, _) => HirPattern::Wildcard,
        Pattern::LiteralBool(b, _, _) => HirPattern::LiteralBool(*b),
        Pattern::LiteralChar(c, _, _) => HirPattern::LiteralInt(i64::from(u32::from(*c))),
        Pattern::LiteralInt(n, _, _) => HirPattern::LiteralInt(*n as i64),
        Pattern::Binding { name, .. } => {
            if infer_variant && infers_fieldless_variant(ctx, ty, name) {
                let (Type::Named(enum_name) | Type::Generic(enum_name, _)) = ty else {
                    unreachable!("only enum types infer variants");
                };
                HirPattern::EnumVariant {
                    enum_name: enum_name.clone().into(),
                    variant: name.clone(),
                }
            } else {
                HirPattern::Binding {
                    name: ctx.bind(name.clone(), ty.clone()),
                    ty: ty.clone().into(),
                }
            }
        }
        Pattern::EnumVariant { variant, .. } => {
            let Some((enum_name, _)) = enum_variant_payload(ctx, ty, variant) else {
                return Err(unsupported(span, "enum pattern on a non-enum scrutinee"));
            };
            HirPattern::EnumVariant {
                enum_name: enum_name.into(),
                variant: variant.clone(),
            }
        }
        Pattern::EnumVariantTuple {
            variant, fields, ..
        } => bind_variant_tuple(
            ty,
            variant,
            PayloadFields::Patterns(fields),
            infer_variant,
            span,
            ctx,
            nested,
        )?,
        Pattern::ClassDowncast {
            class_name,
            binding,
            ..
        } => {
            // Unqualified variant like `Some(x)` if it names a variant of the
            // scrutinee's enum; otherwise a real interface downcast.
            if infer_variant
                && matches!(
                    enum_variant_payload(ctx, ty, class_name),
                    Some((_, Some(_)))
                )
            {
                bind_variant_tuple(
                    ty,
                    class_name,
                    PayloadFields::Name(binding),
                    infer_variant,
                    span,
                    ctx,
                    nested,
                )?
            } else {
                // Pattern spellings are unit-local, just like annotations.
                // Both the runtime class test and its payload local must use
                // the canonical identity used by checked field/method uses.
                let binding_ty = ctx.tables.normalize(&Type::Named(class_name.clone()));
                let Type::Named(canonical_class) = &binding_ty else {
                    return Err(unsupported(span, "non-class downcast target"));
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
    })
}

/// Build an `EnumVariantTuple` pattern, binding each payload position to the
/// variant's (substituted) payload type. `_` positions match without binding;
/// nested positions bind a synthetic local queued on `nested`.
fn bind_variant_tuple<'p>(
    ty: &Type,
    variant: &str,
    fields: PayloadFields<'p>,
    infer_variant: bool,
    span: Span,
    ctx: &mut LowerCtx,
    nested: &mut Vec<PendingPattern<'p>>,
) -> Result<HirPattern, Diagnostic> {
    use crate::parser::ast::Pattern;
    let Some((enum_name, payload)) = enum_variant_payload(ctx, ty, variant) else {
        return Err(unsupported(span, "enum pattern on a non-enum scrutinee"));
    };
    let Some(payload) = payload else {
        return Err(unsupported(span, "unknown enum variant in pattern"));
    };
    let arity = match fields {
        PayloadFields::Patterns(fields) => fields.len(),
        PayloadFields::Name(_) => 1,
    };
    if payload.len() != arity {
        return Err(unsupported(span, "enum pattern arity mismatch"));
    }
    let mut bindings = Vec::with_capacity(arity);
    for (index, payload_ty) in payload.into_iter().enumerate() {
        let name = match fields {
            PayloadFields::Name(name) if name == "_" => name.clone(),
            PayloadFields::Name(name) => ctx.bind(name.clone(), payload_ty.clone()),
            PayloadFields::Patterns(fields) => match &fields[index] {
                Pattern::Wildcard(_, _) => "_".to_string(),
                Pattern::Binding { name, .. }
                    if !(infer_variant && infers_fieldless_variant(ctx, &payload_ty, name)) =>
                {
                    ctx.bind(name.clone(), payload_ty.clone())
                }
                sub => {
                    // `$` cannot appear in a source identifier.
                    let local = ctx.bind("$pat".to_string(), payload_ty.clone());
                    nested.push(PendingPattern {
                        local: local.clone(),
                        ty: payload_ty.clone(),
                        pattern: sub,
                        span,
                    });
                    local
                }
            },
        };
        bindings.push((name, payload_ty.into()));
    }
    Ok(HirPattern::EnumVariantTuple {
        enum_name: enum_name.into(),
        variant: variant.to_string(),
        bindings,
    })
}
