use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::*;
use std::collections::HashSet;

use super::*;

impl TypeChecker {
    pub(super) fn check_try_propagate(&mut self, inner: &Expr, span: Span) -> Type {
        let operand_ty = self.check_expr(inner);

        if let Type::Generic(name, args) = &operand_ty {
            if name == "Option" && args.len() == 1 {
                let some_ty = args[0].clone();
                let return_ty = self.current_return_type.clone();
                match &return_ty {
                    Type::Generic(ret_name, ret_args)
                        if ret_name == "Option" && ret_args.len() == 1 =>
                    {
                        return some_ty;
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1807,
                                format!(
                                    "`?` on `Option<T>` can only be used inside a function returning `Option<U>`, found `{}`",
                                    type_name(other)
                                ),
                            )
                            .with_label(Label::primary(span, "invalid context for Option `?`"))
                            .with_help("change the function return type to `Option<U>`"),
                        );
                        return some_ty;
                    }
                }
            }
        }

        // Otherwise the operand must be Result<T,E>.
        let (ok_ty, err_ty) = match &operand_ty {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                (args[0].clone(), args[1].clone())
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1806,
                        format!(
                            "the `?` operator requires `Result<T,E>` or `Option<T>`, found `{}`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "not a Result or Option"))
                    .with_help(
                        "wrap the value in `Result::Ok(...)`, `Result::Err(...)`, or `Option::Some(...)`",
                    ),
                );
                return Type::Void;
            }
        };

        // The enclosing function must return Result<U,E> with matching error type
        let return_ty = self.current_return_type.clone();
        match &return_ty {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                if args[1] == err_ty || args[1] == Type::Void || err_ty == Type::Void {
                    // ok_ty is the success value type
                    ok_ty
                } else if self.err_converts_via_into(&err_ty, &args[1]) {
                    // Automatic error conversion (willow-1ow): the operand error
                    // `E1` implements `Into<E2>`, so `?` converts `E1 -> E2` on
                    // the Err early-return path. Codegen emits `e1.into()`.
                    ok_ty
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1805,
                            format!(
                                "error type mismatch: function returns `Result<_, {}>` but `?` propagates `{}`",
                                type_name(&args[1]),
                                type_name(&err_ty)
                            ),
                        )
                        .with_label(Label::primary(span, "error type mismatch"))
                        .with_help(format!(
                            "implement `Into<{}>` on `{}` to allow `?` to convert the error",
                            type_name(&args[1]),
                            type_name(&err_ty)
                        )),
                    );
                    ok_ty
                }
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1807,
                        format!(
                            "`?` can only be used inside a function returning `Result<T,E>`, found `{}`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "invalid context for `?`"))
                    .with_help("change the function return type to `Result<T, E>`"),
                );
                ok_ty
            }
        }
    }

    pub(super) fn check_lambda(&mut self, l: &LambdaExpr) -> Type {
        self.check_lambda_with_context(l, None, None)
    }

    pub(super) fn check_lambda_expecting(&mut self, l: &LambdaExpr, expected: &Type) -> Type {
        let Type::Fn(params, ret) = expected else {
            return self.check_lambda(l);
        };
        if params.len() != l.params.len() {
            return self.check_lambda(l);
        }
        self.check_lambda_with_context(l, Some(params.as_slice()), Some(ret.as_ref()))
    }

    pub(super) fn check_lambda_with_param_context(
        &mut self,
        l: &LambdaExpr,
        expected_params: &[Type],
    ) -> Type {
        if expected_params.len() != l.params.len() {
            return self.check_lambda(l);
        }
        self.check_lambda_with_context(l, Some(expected_params), None)
    }

    pub(super) fn check_lambda_with_context(
        &mut self,
        l: &LambdaExpr,
        expected_params: Option<&[Type]>,
        expected_return: Option<&Type>,
    ) -> Type {
        // Params may be annotated directly or inferred from an expected fn type.
        let mut param_types = Vec::new();
        for (idx, p) in l.params.iter().enumerate() {
            match &p.ty {
                Some(ty) => {
                    self.validate_type(ty, p.span);
                    param_types.push(ty.clone());
                }
                None => {
                    if let Some(expected_ty) = expected_params.and_then(|params| params.get(idx)) {
                        self.validate_type(expected_ty, p.span);
                        param_types.push(expected_ty.clone());
                    } else {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1001,
                                format!("cannot infer type for lambda parameter `{}`", p.name),
                            )
                            .with_label(Label::primary(p.span, "type annotation required"))
                            .with_help("add a parameter type, e.g. `|x: i64|`"),
                        );
                        param_types.push(Type::I64); // recover
                    }
                }
            }
        }

        // Determine expected return type from annotation or call-site context.
        let expected_ret = l.return_type.as_ref().or(expected_return);
        if let Some(ret) = expected_ret {
            self.validate_type(ret, l.span);
        }

        // Type-check the body with params in scope.
        self.symbols.push_scope();
        for (p, ty) in l.params.iter().zip(&param_types) {
            self.symbols.define_var(
                p.name.clone(),
                crate::semantic::symbols::VarInfo {
                    ty: ty.clone(),
                    mutable: false,
                    is_param: true,
                    declaration_span: p.span,
                },
            );
        }

        // Save/restore outer return type so `return` stmts in the lambda body
        // are checked against the lambda's return type, not the enclosing function's.
        let saved_ret_ty = self.current_return_type.clone();

        let body_ty = match &l.body {
            LambdaBody::Expr(e) => self.check_expr(e),
            LambdaBody::Block(b) => {
                if let Some(ann) = expected_ret {
                    // Annotation provided: validate return stmts against it.
                    self.current_return_type = ann.clone();
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    ann.clone()
                } else {
                    // No annotation: collect the return type via the lambda stack.
                    self.lambda_return_stack.push(None);
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    let inferred = self
                        .lambda_return_stack
                        .pop()
                        .flatten()
                        .unwrap_or(Type::Void);
                    inferred
                }
            }
        };
        self.current_return_type = saved_ret_ty;
        self.symbols.pop_scope();

        let ret_ty = match &l.return_type {
            Some(ann) => {
                if !self.types_compatible(ann, &body_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(ann, &body_ty),
                            format!(
                                "lambda return type mismatch: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&body_ty)
                            ),
                        )
                        .with_label(Label::primary(l.span, "return type mismatch")),
                    );
                }
                ann.clone()
            }
            None => {
                if let Some(expected) = expected_return {
                    if !self.types_compatible(expected, &body_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                self.type_mismatch_error_code(expected, &body_ty),
                                format!(
                                    "lambda return type mismatch: expected `{}`, found `{}`",
                                    type_name(expected),
                                    type_name(&body_ty)
                                ),
                            )
                            .with_label(Label::primary(l.span, "return type mismatch")),
                        );
                    }
                    expected.clone()
                } else {
                    body_ty
                }
            }
        };

        // Record the inferred return type so the backend can use it without
        // falling back to I64 when no explicit annotation is present.
        self.lambda_return_types.insert(l.span, ret_ty.clone());

        let fn_ty = Type::Fn(param_types, Box::new(ret_ty));
        self.lambda_fn_types.insert(l.span, fn_ty.clone());
        fn_ty
    }

    /// Reinterpret an unqualified match pattern as an enum-variant pattern when
    /// the scrutinee is an enum with that variant (willow-60o.1): `Ok(v)` parses
    /// as `ClassDowncast` → `EnumVariantTuple`; a bare `Closed` parses as
    /// `Binding` → `EnumVariant` (only for a fieldless variant; otherwise it is a
    /// genuine catch-all binding). Returns `None` when no reinterpretation
    /// applies.
    pub(super) fn normalize_match_pattern(
        &self,
        pattern: &Pattern,
        scrutinee_ty: &Type,
    ) -> Option<Pattern> {
        let enum_name = match scrutinee_ty {
            Type::Named(n) | Type::Generic(n, _) => n,
            _ => return None,
        };
        let info = self.symbols.lookup_enum(enum_name)?;
        match pattern {
            Pattern::ClassDowncast {
                class_name,
                binding,
                span,
            } => {
                let variant = info.variants.iter().find(|v| v.name == *class_name)?;
                if variant.payload_types.is_empty() {
                    return None;
                }
                Some(Pattern::EnumVariantTuple {
                    enum_name: enum_name.clone(),
                    variant: class_name.clone(),
                    bindings: vec![binding.clone()],
                    span: *span,
                })
            }
            Pattern::Binding { name, span } => {
                let variant = info.variants.iter().find(|v| v.name == *name)?;
                if !variant.payload_types.is_empty() {
                    return None;
                }
                Some(Pattern::EnumVariant {
                    enum_name: enum_name.clone(),
                    variant: name.clone(),
                    span: *span,
                })
            }
            _ => None,
        }
    }

    pub(super) fn check_match_expr(&mut self, m: &MatchExpr) -> Type {
        let scrutinee_ty = self.check_expr(&m.scrutinee);

        if m.arms.is_empty() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1202,
                    "match expression has no arms",
                )
                .with_label(Label::primary(m.span, "no arms in match")),
            );
            return Type::Void;
        }

        let mut covered_variants: HashSet<String> = HashSet::new();
        let mut has_wildcard = false;
        let mut has_true = false;
        let mut has_false = false;
        let mut result_type: Option<Type> = None;
        let mut found_unreachable = false;

        for arm in &m.arms {
            // Check if arm is unreachable (after a wildcard/binding)
            if has_wildcard && !found_unreachable {
                self.push(
                    Diagnostic::new(Severity::Warning, ErrorCode::W1201, "unreachable match arm")
                        .with_label(Label::primary(arm.span, "this arm is unreachable")),
                );
                found_unreachable = true;
            }

            // Reinterpret `Ok(v)` / `Closed` as enum-variant patterns when the
            // scrutinee is an enum, and record the reinterpretation for the
            // backend (willow-60o.1). Everything below uses `pattern`.
            let reinterpreted = self.normalize_match_pattern(&arm.pattern, &scrutinee_ty);
            if let Some(p) = &reinterpreted {
                self.pattern_resolutions
                    .insert(arm.pattern.span(), p.clone());
            }
            let pattern: &Pattern = reinterpreted.as_ref().unwrap_or(&arm.pattern);

            // Validate pattern and track coverage
            match pattern {
                Pattern::Wildcard(_) => {
                    has_wildcard = true;
                }
                Pattern::Binding { .. } => {
                    has_wildcard = true; // binding covers everything
                }
                Pattern::LiteralBool(b, span) => {
                    if scrutinee_ty != Type::Bool {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "bool pattern cannot match scrutinee of type `{}`",
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "pattern type mismatch")),
                        );
                    }
                    if *b {
                        has_true = true;
                    } else {
                        has_false = true;
                    }
                }
                Pattern::LiteralInt(_, span) => {
                    if scrutinee_ty != Type::I64 {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "integer pattern cannot match scrutinee of type `{}`",
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "pattern type mismatch")),
                        );
                    }
                }
                Pattern::EnumVariant {
                    enum_name,
                    variant,
                    span,
                } => {
                    // Generic enum variant patterns: the scrutinee may be
                    // Generic(enum_name, type_args) rather than Named(enum_name).
                    let is_builtin_match = matches!(&scrutinee_ty,
                        Type::Generic(n, _) if n == enum_name
                    );
                    // Verify enum_name matches scrutinee type
                    if !is_builtin_match {
                        match &scrutinee_ty {
                            Type::Named(sname) if sname == enum_name => {}
                            _ => {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1205,
                                        format!(
                                            "enum pattern `{}::{}` cannot match scrutinee of type `{}`",
                                            enum_name,
                                            variant,
                                            type_name(&scrutinee_ty)
                                        ),
                                    )
                                    .with_label(Label::primary(*span, "pattern type mismatch")),
                                );
                            }
                        }
                        // Verify variant exists
                        let variant_valid = self
                            .symbols
                            .lookup_enum(enum_name)
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .is_some();
                        if !variant_valid {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E1208,
                                    format!("no variant `{}` in enum `{}`", variant, enum_name),
                                )
                                .with_label(Label::primary(*span, "unknown enum variant")),
                            );
                        }
                    }
                    covered_variants.insert(variant.clone());
                }
                Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    span,
                } => {
                    // Generic enum variant: resolve concrete payload types from scrutinee.
                    let builtin_payload: Option<Vec<Type>> =
                        self.resolve_generic_variant_payload(enum_name, variant, &scrutinee_ty);

                    if let Some(ref pts) = builtin_payload {
                        // Built-in generic variant — validate binding count
                        if bindings.len() != pts.len() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E1209,
                                    format!(
                                        "variant `{}::{}` expects {} field(s), found {}",
                                        enum_name,
                                        variant,
                                        pts.len(),
                                        bindings.len()
                                    ),
                                )
                                .with_label(Label::primary(*span, "wrong number of bindings")),
                            );
                        }
                    } else {
                        // User-defined enum variant
                        match &scrutinee_ty {
                            Type::Named(sname) if sname == enum_name => {}
                            _ => {
                                self.push(Diagnostic::new(Severity::Error, ErrorCode::E1205,
                                    format!("enum pattern `{}::{}(..)` cannot match scrutinee of type `{}`",
                                        enum_name, variant, type_name(&scrutinee_ty)))
                                    .with_label(Label::primary(*span, "pattern type mismatch")));
                            }
                        }
                        let payload_types = self
                            .symbols
                            .lookup_enum(enum_name)
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .map(|v| v.payload_types.clone());
                        match payload_types {
                            None => {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1208,
                                        format!("no variant `{}` in enum `{}`", variant, enum_name),
                                    )
                                    .with_label(Label::primary(*span, "unknown enum variant")),
                                );
                            }
                            Some(ref pts) => {
                                if pts.is_empty() {
                                    self.push(
                                        Diagnostic::new(
                                            Severity::Error,
                                            ErrorCode::E1209,
                                            format!(
                                                "variant `{}::{}` has no payload; remove `(..)`",
                                                enum_name, variant
                                            ),
                                        )
                                        .with_label(
                                            Label::primary(
                                                *span,
                                                "fieldless variant used with payload pattern",
                                            ),
                                        ),
                                    );
                                } else if bindings.len() != pts.len() {
                                    self.push(
                                        Diagnostic::new(
                                            Severity::Error,
                                            ErrorCode::E1209,
                                            format!(
                                                "variant `{}::{}` expects {} field(s), found {}",
                                                enum_name,
                                                variant,
                                                pts.len(),
                                                bindings.len()
                                            ),
                                        )
                                        .with_label(
                                            Label::primary(*span, "wrong number of bindings"),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    covered_variants.insert(variant.clone());
                }
                Pattern::ClassDowncast {
                    class_name, span, ..
                } => {
                    // `Dog(d)` downcasts an interface scrutinee to a concrete
                    // class. The scrutinee must be an interface, and the class
                    // must implement it (else the arm can never match).
                    // Class patterns do not contribute to exhaustiveness, so a
                    // wildcard arm is still required.
                    let scrut_is_interface = matches!(&scrutinee_ty,
                        Type::Named(n) | Type::Generic(n, _)
                            if self.symbols.lookup_interface(n).is_some());
                    if !scrut_is_interface {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "class pattern `{}(..)` requires an interface scrutinee, found `{}`",
                                    class_name,
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "scrutinee is not an interface"))
                            .with_help("match on a value of interface type to downcast to a class"),
                        );
                    } else if self.symbols.lookup_class(class_name).is_none() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0350,
                                format!("cannot find class `{class_name}`"),
                            )
                            .with_label(Label::primary(*span, "unknown class in pattern")),
                        );
                    } else if !self.class_implements_interface(class_name, &scrutinee_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0415,
                                format!(
                                    "class `{}` does not implement `{}`, so this pattern can never match",
                                    class_name,
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "unrelated class")),
                        );
                    }
                }
            }

            // Check arm body in a new scope
            self.symbols.push_scope();
            // For EnumVariantTuple: bind payload variables in arm scope
            if let Pattern::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
                ..
            } = pattern
            {
                // Resolve payload types: first check built-in generic types
                // Resolve concrete payload types: use generic instantiation when available.
                let payload_types: Vec<Type> = self
                    .resolve_generic_variant_payload(enum_name, variant, &scrutinee_ty)
                    .unwrap_or_default();
                for (binding, ty) in bindings.iter().zip(payload_types.iter()) {
                    self.symbols.define_var(
                        binding.clone(),
                        VarInfo {
                            ty: ty.clone(),
                            mutable: false,
                            is_param: false,
                            declaration_span: pattern.span(),
                        },
                    );
                }
            }
            // For a class downcast pattern, bind the downcast value as the
            // concrete class (willow-1js.4). `_` does not bind.
            if let Pattern::ClassDowncast {
                class_name,
                binding,
                span: bspan,
            } = pattern
                && binding != "_"
            {
                self.symbols.define_var(
                    binding.clone(),
                    VarInfo {
                        ty: Type::Named(class_name.clone()),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            // For binding patterns, define the variable
            if let Pattern::Binding { name, span: bspan } = pattern {
                self.symbols.define_var(
                    name.clone(),
                    VarInfo {
                        ty: scrutinee_ty.clone(),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            let arm_ty = self.check_match_body(&arm.body);
            self.symbols.pop_scope();

            // Never arms don't constrain result type
            if arm_ty == Type::Never {
                continue;
            }

            // When the new arm type is a partial-generic (e.g. `Option<void>` from `None`),
            // keep the richer type already recorded rather than replacing it.
            let arm_ty = match (&result_type, &arm_ty) {
                (Some(existing), arm) if self.generic_partially_matches(existing, arm) => {
                    existing.clone()
                }
                (Some(existing), arm) if self.generic_partially_matches(arm, existing) => {
                    arm.clone()
                }
                _ => arm_ty,
            };

            match &result_type {
                None => result_type = Some(arm_ty),
                Some(existing) => {
                    if !self.types_compatible(existing, &arm_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1201,
                                format!(
                                    "match arms have incompatible types: `{}` and `{}`",
                                    type_name(existing),
                                    type_name(&arm_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arm.span,
                                format!("found `{}`", type_name(&arm_ty)),
                            )),
                        );
                    }
                }
            }
        }

        // Exhaustiveness check
        if !has_wildcard {
            match &scrutinee_ty {
                Type::Bool => {
                    if !has_true || !has_false {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1207,
                                "non-exhaustive match: missing bool patterns",
                            )
                            .with_label(Label::primary(m.span, "match is not exhaustive"))
                            .with_help("add `true` and `false` patterns, or use a wildcard `_`"),
                        );
                    }
                }
                Type::I64 => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1206,
                            "non-exhaustive match on `i64`: add a wildcard arm `_ => ...`",
                        )
                        .with_label(Label::primary(m.span, "match is not exhaustive")),
                    );
                }
                Type::Named(enum_name) => {
                    if let Some(enum_info) = self.symbols.lookup_enum(enum_name).cloned() {
                        for variant in &enum_info.variants {
                            if !covered_variants.contains(&variant.name) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1202,
                                        format!(
                                            "non-exhaustive match: variant `{}::{}` not covered",
                                            enum_name, variant.name
                                        ),
                                    )
                                    .with_label(Label::primary(m.span, "match is not exhaustive")),
                                );
                            }
                        }
                    }
                }
                // Generic enum: check all variants are covered.
                // Uses registered enum info so any stdlib or user generic enum is handled.
                Type::Generic(enum_name, _) => {
                    if let Some(enum_info) = self.symbols.lookup_enum(enum_name).cloned() {
                        for variant in &enum_info.variants {
                            if !covered_variants.contains(&variant.name) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1202,
                                        format!(
                                            "non-exhaustive match: variant `{}::{}` not covered",
                                            enum_name, variant.name
                                        ),
                                    )
                                    .with_label(Label::primary(m.span, "match is not exhaustive"))
                                    .with_help("add the missing variant or use a wildcard `_` arm"),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        result_type.unwrap_or(Type::Void)
    }

    pub(super) fn check_match_body(&mut self, body: &MatchBody) -> Type {
        match body {
            MatchBody::Expr(expr) => self.check_expr(expr),
            MatchBody::Block(block) => {
                self.check_block(block);
                Type::Void
            }
        }
    }
}
