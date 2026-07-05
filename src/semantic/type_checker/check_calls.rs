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
        let ty = self.check_expr_expecting_inner(expr, expected);
        self.expr_types.insert(expr.span(), ty.clone());
        ty
    }

    fn check_expr_expecting_inner(&mut self, expr: &Expr, expected: &Type) -> Type {
        // Contextually-typed lambda.
        if let (Expr::Lambda(lambda), Type::Fn(..)) = (expr, expected) {
            return self.check_lambda_expecting(lambda, expected);
        }
        // A ternary propagates the expected type into BOTH branches, so
        // `ok ? Ok(10) : Err("bad")` resolves its unqualified variants against
        // the expected enum exactly like a direct `return` does (willow-ok7f).
        if let Expr::Ternary(t) = expr {
            return self.check_ternary_expecting(t, expected);
        }
        // Unqualified enum-variant construction resolved by the expected type
        // (willow-60o.1): `Ok(42)` (payload) and `Closed`/`None` (fieldless), for
        // both non-generic enums and generic ones (`Result<i64, String>`, ...).
        if let Expr::Call(c) = expr
            && let Some((enum_name, payloads, result)) = self.expected_variant(&c.callee, expected)
        {
            return self.construct_variant_call(c, &enum_name, &payloads, result);
        }
        if let Expr::Var(name, span) = expr {
            // A bare identifier resolves to a fieldless variant only when it is
            // not a local variable (a variable shadows the variant).
            if self.symbols.lookup_var(name).is_none()
                && let Some((enum_name, payloads, result)) = self.expected_variant(name, expected)
                && payloads.is_empty()
            {
                self.enum_variant_resolutions.insert(*span, enum_name);
                return result;
            }
        }
        self.check_expr(expr)
    }

    /// Check a ternary whose result flows into an expected type: the condition
    /// stays `bool`, and BOTH branches are checked expecting `expected`, so
    /// contextual forms (unqualified enum variants, contextually-typed lambdas,
    /// nested ternaries) resolve inside the branches (willow-ok7f).
    fn check_ternary_expecting(&mut self, t: &TernaryExpr, expected: &Type) -> Type {
        let cond_ty = self.check_expr(&t.condition);
        if cond_ty != Type::Bool {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0901,
                    format!(
                        "ternary condition must be `bool`, found `{}`",
                        type_name(&cond_ty)
                    ),
                )
                .with_label(Label::primary(
                    t.condition.span(),
                    format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                )),
            );
        }
        let then_ty = self.check_expr_expecting(&t.then_expr, expected);
        let else_ty = self.check_expr_expecting(&t.else_expr, expected);
        if let Some(unified_ty) = self.unify_ternary_types(&then_ty, &else_ty) {
            self.validate_type(&unified_ty, t.span);
            unified_ty
        } else {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0902,
                    format!(
                        "ternary branches have incompatible types: `{}` and `{}`",
                        type_name(&then_ty),
                        type_name(&else_ty)
                    ),
                )
                .with_label(Label::primary(
                    t.else_expr.span(),
                    format!(
                        "expected `{}`, found `{}`",
                        type_name(&then_ty),
                        type_name(&else_ty)
                    ),
                ))
                .with_label(Label::secondary(
                    t.then_expr.span(),
                    format!("this branch has type `{}`", type_name(&then_ty)),
                )),
            );
            Type::Void
        }
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

#[cfg(test)]
mod ternary_expecting_tests {
    //! willow-ok7f: the expected type propagates into ternary branches.
    //! 20 perspectives: 1 return Result, 2 return Option, 3 annotated let,
    //! 4 bare fieldless variants (non-generic), 5 payload variants
    //! (non-generic), 6 mixed qualified/unqualified, 7 nested ternary chain,
    //! 8 call-argument position, 9 contextually-typed lambda branches,
    //! 10 unknown variant still errors, 11 payload type mismatch errors,
    //! 12 non-bool condition still E0901, 13 variant vs non-enum branch
    //! errors, 14 no expected context still errors (pins behavior),
    //! 15 variant + variable branch mix, 16 generic payload (Array),
    //! 17 Option<String>, 18 parenthesized nested ternaries, 19 wrong enum
    //! family still errors, 20 deep nesting in both branches.
    /// Lex + parse + type-check with the prelude registered (the `Option`/
    /// `Result` enums live there), returning the diagnostics.
    fn check_with_prelude(src: &str) -> Vec<crate::diagnostics::Diagnostic> {
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude registers");
        checker.check_program(&program);
        checker.errors
    }

    fn ok(src: &str) {
        let diags = check_with_prelude(src);
        assert!(diags.is_empty(), "expected clean check, got {diags:?}");
    }

    fn errs(src: &str) -> Vec<String> {
        check_with_prelude(src)
            .into_iter()
            .map(|d| format!("{:?} {}", d.code, d.message))
            .collect()
    }

    // 1
    #[test]
    fn t01_return_result_branches() {
        ok("fn f(c: bool) -> Result<i64, String> { return c ? Ok(10) : Err(\"bad\"); }");
    }

    // 2
    #[test]
    fn t02_return_option_branches() {
        ok("fn f(c: bool) -> Option<i64> { return c ? Some(7) : None; }");
    }

    // 3
    #[test]
    fn t03_annotated_let() {
        ok("fn f(c: bool) { let r: Result<i64, String> = c ? Ok(1) : Err(\"e\"); }");
    }

    // 4
    #[test]
    fn t04_bare_fieldless_variants() {
        ok("enum Sig { Go, Stop, } fn f(c: bool) -> Sig { return c ? Go : Stop; }");
    }

    // 5
    #[test]
    fn t05_payload_variants_non_generic() {
        ok("enum Msg { Num(i64), Halt, } fn f(c: bool) -> Msg { return c ? Num(1) : Halt; }");
    }

    // 6
    #[test]
    fn t06_mixed_qualified_unqualified() {
        ok("fn f(c: bool) -> Result<i64, String> { return c ? Result::Ok(1) : Err(\"e\"); }");
    }

    // 7
    #[test]
    fn t07_nested_ternary_chain() {
        ok(
            "fn f(a: bool, b: bool) -> Result<i64, String> { return a ? Ok(1) : b ? Ok(2) : Err(\"e\"); }",
        );
    }

    // 8
    #[test]
    fn t08_call_argument_position() {
        ok("fn take(r: Result<i64, String>) -> i64 { return 0; } \
            fn f(c: bool) -> i64 { return take(c ? Ok(1) : Err(\"e\")); }");
    }

    // 9
    #[test]
    fn t09_lambda_branches_contextually_typed() {
        ok("fn f(c: bool) { let g: fn(i64) -> i64 = c ? |x| x : |x| x * 2; }");
    }

    // 10
    #[test]
    fn t10_unknown_variant_errors() {
        let e = errs("fn f(c: bool) -> Result<i64, String> { return c ? Okk(1) : Err(\"e\"); }");
        assert!(!e.is_empty(), "unknown variant must error");
    }

    // 11
    #[test]
    fn t11_payload_type_mismatch_errors() {
        let e = errs("fn f(c: bool) -> Result<i64, String> { return c ? Ok(\"s\") : Err(\"e\"); }");
        assert!(!e.is_empty(), "payload mismatch must error");
    }

    // 12
    #[test]
    fn t12_non_bool_condition_errors() {
        let e = errs("fn f() -> Result<i64, String> { return 1 ? Ok(1) : Err(\"e\"); }");
        assert!(e.iter().any(|m| m.contains("E0901")), "{e:?}");
    }

    // 13
    #[test]
    fn t13_variant_vs_non_enum_branch_errors() {
        let e = errs("fn f(c: bool) -> Result<i64, String> { return c ? Ok(1) : 5; }");
        assert!(!e.is_empty(), "incompatible branches must error");
    }

    // 14
    #[test]
    fn t14_no_expected_context_still_errors() {
        // Without an expected type there is nothing to resolve the variants
        // against; this pins the current (rejecting) behavior.
        let e = errs("fn f(c: bool) { let x = c ? Ok(1) : Err(\"e\"); }");
        assert!(!e.is_empty());
    }

    // 15
    #[test]
    fn t15_variant_and_variable_branch_mix() {
        ok(
            "fn f(c: bool, fallback: Result<i64, String>) -> Result<i64, String> { \
            return c ? Ok(1) : fallback; }",
        );
    }

    // 16
    #[test]
    fn t16_generic_array_payload() {
        ok("import std::collections::Array; \
            fn f(c: bool) -> Result<Array<i64>, String> { \
            let xs: Array<i64> = [1, 2]; return c ? Ok(xs) : Err(\"e\"); }");
    }

    // 17
    #[test]
    fn t17_option_string() {
        ok("fn f(c: bool) -> Option<String> { return c ? Some(\"x\") : None; }");
    }

    // 18
    #[test]
    fn t18_parenthesized_nested() {
        ok("fn f(a: bool, b: bool) -> Result<i64, String> { \
            return a ? (b ? Ok(1) : Err(\"x\")) : Err(\"y\"); }");
    }

    // 19
    #[test]
    fn t19_wrong_enum_family_errors() {
        let e = errs("fn f(c: bool) -> Result<i64, String> { return c ? Some(1) : None; }");
        assert!(!e.is_empty(), "Option variants against Result must error");
    }

    // 20
    #[test]
    fn t20_deep_nesting_both_branches() {
        ok("fn f(a: bool, b: bool, c: bool) -> Option<i64> { \
            return a ? (b ? Some(1) : None) : (c ? Some(2) : None); }");
    }
}
