use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::*;

use super::*;

impl TypeChecker {
    pub(super) fn check_call_argument_count(
        &mut self,
        callee: &str,
        expected: usize,
        supplied: usize,
        span: Span,
    ) {
        if expected != supplied {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "{} takes {} argument(s) but {} were supplied",
                        callee, expected, supplied
                    ),
                )
                .with_label(Label::primary(span, "wrong number of arguments")),
            );
        }
    }

    pub(super) fn check_value_call_args(&mut self, params: &[Type], args: &[CallArg]) {
        let param_infos = value_param_infos(params);
        self.check_call_args_against_param_infos(&param_infos, args);
    }

    pub(super) fn check_call_args_against_param_infos(
        &mut self,
        params: &[ParamInfo],
        args: &[CallArg],
    ) {
        for (param, arg) in params.iter().zip(args) {
            self.check_call_arg_against_param(param, arg);
        }
        self.check_mut_reference_aliases(params, args);
    }

    pub(super) fn check_call_arg_against_param(&mut self, param: &ParamInfo, arg: &CallArg) {
        match (&param.mode, &arg.mode) {
            (ParamMode::Value, CallArgMode::Value) => {
                self.check_value_arg_type(&param.ty, arg);
            }
            (ParamMode::Value, CallArgMode::Reference { .. }) => {
                let arg_ty = self.check_expr(&arg.expr);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1703,
                        "unexpected reference argument",
                    )
                    .with_label(Label::primary(
                        arg.span,
                        format!(
                            "parameter expects `{}`, not `& {}`",
                            type_name(&param.ty),
                            type_name(&arg_ty)
                        ),
                    )),
                );
            }
            (ParamMode::Reference { .. }, CallArgMode::Value) => {
                self.check_expr(&arg.expr);
                let expr_span = arg.expr.span();
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1702,
                    "expected reference argument for reference parameter",
                )
                .with_label(Label::primary(
                    expr_span,
                    "expected `&` before this argument",
                ))
                .with_help("pass the mutable place by reference");

                if let Expr::Var(name, _) = &arg.expr {
                    diagnostic = diagnostic.with_help(format!("write `&{}`", name));
                    diagnostic = diagnostic.with_fix(FixSuggestion::insertion(
                        Span::new(
                            expr_span.start,
                            expr_span.start,
                            expr_span.line,
                            expr_span.col,
                        ),
                        "&",
                        "pass the variable by reference",
                    ));
                }

                self.push(diagnostic);
            }
            (ParamMode::Reference { mutable, .. }, CallArgMode::Reference { .. }) => {
                self.check_reference_argument(param, arg, *mutable);
            }
        }
    }

    pub(super) fn check_value_arg_type(&mut self, param_ty: &Type, arg: &CallArg) {
        // Route through `check_expr_expecting` so an unqualified enum-variant
        // construction in argument position (`unwrap_or_zero(Ok(42))`) resolves
        // against the parameter type, just like a contextually-typed lambda
        // (willow-60o.1).
        let arg_ty = self.check_expr_expecting(&arg.expr, param_ty);
        if !self.types_compatible(param_ty, &arg_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    self.type_mismatch_error_code(param_ty, &arg_ty),
                    format!(
                        "mismatched types: expected `{}`, found `{}`",
                        type_name(param_ty),
                        type_name(&arg_ty)
                    ),
                )
                .with_label(Label::primary(
                    arg.expr.span(),
                    format!("expected `{}`", type_name(param_ty)),
                )),
            );
        }
    }

    pub(super) fn check_fn_arg_with_param_context(
        &mut self,
        expr: &Expr,
        expected_params: &[Type],
    ) -> Type {
        match expr {
            Expr::Lambda(lambda) => self.check_lambda_with_param_context(lambda, expected_params),
            _ => self.check_expr(expr),
        }
    }

    pub(super) fn check_expr_expecting(&mut self, expr: &Expr, expected: &Type) -> Type {
        // Contextually-typed lambda.
        if let (Expr::Lambda(lambda), Type::Fn(..)) = (expr, expected) {
            return self.check_lambda_expecting(lambda, expected);
        }
        // Unqualified enum-variant construction resolved by the expected type
        // (willow-60o.1): `Ok(42)` (payload) and `Closed`/`None` (fieldless), for
        // both non-generic enums and generic ones (`Result<i64, String>`, ...).
        if let Expr::Call(c) = expr {
            if let Some((enum_name, payloads, result)) = self.expected_variant(&c.callee, expected)
            {
                return self.construct_variant_call(c, &enum_name, &payloads, result);
            }
        }
        if let Expr::Var(name, span) = expr {
            // A bare identifier resolves to a fieldless variant only when it is
            // not a local variable (a variable shadows the variant).
            if self.symbols.lookup_var(name).is_none() {
                if let Some((enum_name, payloads, result)) = self.expected_variant(name, expected) {
                    if payloads.is_empty() {
                        self.enum_variant_resolutions.insert(*span, enum_name);
                        return result;
                    }
                }
            }
        }
        self.check_expr(expr)
    }

    /// If `expected` is an enum type with a variant `name`, return the enum's
    /// name, the variant's payload types (instantiated for a generic enum's type
    /// arguments), and the enum type to yield (willow-60o.1).
    pub(super) fn expected_variant(
        &self,
        name: &str,
        expected: &Type,
    ) -> Option<(String, Vec<Type>, Type)> {
        match expected {
            Type::Named(enum_name) => {
                let info = self.symbols.lookup_enum(enum_name)?;
                if !info.type_params.is_empty() {
                    return None;
                }
                let variant = info.variants.iter().find(|v| v.name == name)?;
                Some((
                    enum_name.clone(),
                    variant.payload_types.clone(),
                    Type::Named(enum_name.clone()),
                ))
            }
            Type::Generic(enum_name, type_args) => {
                let info = self.symbols.lookup_enum(enum_name)?;
                if info.type_params.is_empty() {
                    return None;
                }
                let instantiated = info.instantiate(type_args);
                let variant = instantiated.variants.iter().find(|v| v.name == name)?;
                Some((
                    enum_name.clone(),
                    variant.payload_types.clone(),
                    expected.clone(),
                ))
            }
            _ => None,
        }
    }

    /// Validate an unqualified variant construction `name(args)` against the
    /// (possibly instantiated) payload types, record the resolution for the
    /// backend, and yield `result` (the enum type) (willow-60o.1).
    pub(super) fn construct_variant_call(
        &mut self,
        c: &CallExpr,
        enum_name: &str,
        payload_types: &[Type],
        result: Type,
    ) -> Type {
        if c.args.len() != payload_types.len() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "enum variant `{}::{}` takes {} argument(s), got {}",
                        enum_name,
                        c.callee,
                        payload_types.len(),
                        c.args.len()
                    ),
                )
                .with_label(Label::primary(c.span, "wrong number of arguments")),
            );
        }
        for (param_ty, arg) in payload_types.iter().zip(c.args.iter()) {
            let arg_ty = self.check_expr(&arg.expr);
            if !self.types_compatible(param_ty, &arg_ty) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "mismatched types: expected `{}`, found `{}`",
                            type_name(param_ty),
                            type_name(&arg_ty)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong argument type")),
                );
            }
        }
        self.enum_variant_resolutions
            .insert(c.span, enum_name.to_string());
        result
    }

    /// The class in `class_name`'s hierarchy (itself first, then ancestors) that
    /// declares `method` — i.e. the body a call resolves to. `None` if no class
    /// in the chain declares it.
    pub(super) fn resolved_method_class(&self, class_name: &str, method: &str) -> Option<String> {
        let mut current = Some(class_name.to_string());
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let class = self.symbols.lookup_class(&name)?;
            if class.methods.contains_key(method) {
                return Some(name);
            }
            current = class.base_class.clone();
        }
        None
    }

    /// E0810 for a looping method reached through a typed NON-`self` receiver in
    /// a task context (`obj.heavy()` where `obj: Work` and `Work::heavy` loops).
    /// `self.heavy()`, free-function, and static-method calls are handled by the
    /// AST-level `ConcurrencyAnalyzer`; this covers the typed-receiver case it
    /// cannot resolve, so the two never overlap (willow-0a6k.2).
    pub(super) fn check_task_method_call(&mut self, obj_ty: &Type, m: &MethodCallExpr) {
        if !self.current_async_context {
            return;
        }
        if matches!(&m.object, Expr::Var(name, _) if name == "self") {
            return;
        }
        let Type::Named(class) = obj_ty else {
            return;
        };
        // Resolve to the class that actually DECLARES the method (walking the
        // base chain), so an override on a subclass is judged on its own body —
        // a loop-free override must not inherit the base's E0810, and an
        // inherited looping method is attributed to the base that defines it.
        let Some(declaring) = self.resolved_method_class(class, &m.method) else {
            return;
        };
        let key = crate::semantic::ids::FunctionId::method(
            crate::semantic::ids::TypeId::from_source_name(&declaring),
            m.method.as_str(),
        );
        let diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0810,
            format!("sync helper `{key}` with a loop is not preemptible in task context"),
        )
        .with_label(Label::primary(
            m.span,
            "this call can monopolize the scheduler worker",
        ));
        if let Some(&helper_span) = self.nonpreemptible_methods.get(&key) {
            self.push(
                diagnostic
                    .with_label(Label::secondary(
                        helper_span,
                        "this helper contains or reaches a synchronous loop",
                    ))
                    .with_help("make the helper async so its loop can use resumable safepoints"),
            );
        } else if let Some(module) = self.nonpreemptible_module_methods.get(&key) {
            // Defined in another file the entry map cannot render — use a note.
            self.push(
                diagnostic
                    .with_note(format!(
                        "`{key}` is defined in imported module `{module}` and contains or reaches a synchronous loop",
                    ))
                    .with_help("make the helper async so its loop can use resumable safepoints"),
            );
        }
    }

    pub(super) fn check_reference_argument(
        &mut self,
        param: &ParamInfo,
        arg: &CallArg,
        require_mutable: bool,
    ) {
        let Some(place) = self.reference_place_info(&arg.expr, arg.span) else {
            return;
        };

        if require_mutable && !place.mutable {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1701,
                format!("cannot pass immutable variable `{}` as `&mut`", place.name),
            )
            .with_label(Label::primary(
                arg.span,
                "cannot pass immutable variable by mutable reference",
            ))
            .with_label(Label::secondary(
                place.declaration_span,
                "declared immutable here",
            ))
            .with_help("declare the variable as mutable");

            if !place.is_param {
                let decl = place.declaration_span;
                let insert_span =
                    Span::new(decl.start + 4, decl.start + 4, decl.line, decl.col + 4);
                diagnostic = diagnostic.with_fix(FixSuggestion::insertion(
                    insert_span,
                    "mut ",
                    "add `mut` here",
                ));
            }

            self.push(diagnostic);
        }

        if place.ty != param.ty {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1705,
                "reference argument type mismatch",
            )
            .with_label(Label::primary(
                arg.span,
                format!("found `{}`", type_name(&place.ty)),
            ));

            if param.type_span != Span::dummy() {
                diagnostic = diagnostic.with_label(Label::secondary(
                    param.type_span,
                    format!("expected `{}`", type_name(&param.ty)),
                ));
            } else {
                diagnostic = diagnostic.with_label(Label::secondary(
                    param.span,
                    format!("expected `{}`", type_name(&param.ty)),
                ));
            }

            self.push(diagnostic);
        }
    }

    pub(super) fn check_mut_reference_aliases(&mut self, params: &[ParamInfo], args: &[CallArg]) {
        let mut seen_mut_refs: Vec<(String, Span)> = Vec::new();
        let mut seen_other_uses: Vec<(String, Span)> = Vec::new();

        for (param, arg) in params.iter().zip(args) {
            let Some(name) = reference_place_key(&arg.expr) else {
                continue;
            };
            let is_mut_reference = matches!(
                (&param.mode, &arg.mode),
                (
                    ParamMode::Reference { mutable: true, .. },
                    CallArgMode::Reference { .. }
                )
            );

            if is_mut_reference {
                for (previous_name, previous_span) in &seen_mut_refs {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "same mutable place passed here",
                        );
                    }
                }
                for (previous_name, previous_span) in &seen_other_uses {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "same place used by another argument",
                        );
                    }
                }
                seen_mut_refs.push((name, arg.span));
            } else {
                for (previous_name, previous_span) in &seen_mut_refs {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "mutable reference passed here",
                        );
                    }
                }
                seen_other_uses.push((name, arg.span));
            }
        }
    }

    pub(super) fn check_format_call(&mut self, c: &CallExpr) -> Type {
        if c.args.len() != 2 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("format expects 2 arguments, got {}", c.args.len()),
                )
                .with_label(Label::primary(c.span, "wrong number of arguments")),
            );
            for arg in &c.args {
                self.check_expr(&arg.expr);
            }
            return Type::String;
        }

        match &c.args[0].expr {
            Expr::String(spec, span) if is_supported_f64_format(spec) => {
                let _ = span;
            }
            Expr::String(spec, span) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1401,
                        format!("invalid format specifier `{}`", spec),
                    )
                    .with_label(Label::primary(*span, "unsupported format specifier"))
                    .with_help("supported f64 formats are `{:.17g}`, `{:.16f}`, and `{:.6f}`"),
                );
            }
            other => {
                self.check_expr(other);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1401,
                        "format specifier must be a string literal",
                    )
                    .with_label(Label::primary(other.span(), "expected string literal"))
                    .with_help("write the format as a literal, e.g. `format(\"{:.6f}\", value)`"),
                );
            }
        }

        let value_ty = self.check_expr(&c.args[1].expr);
        if value_ty != Type::F64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "mismatched types: expected `f64`, found `{}`",
                        type_name(&value_ty)
                    ),
                )
                .with_label(Label::primary(c.args[1].expr.span(), "expected `f64`")),
            );
        }

        Type::String
    }
}
