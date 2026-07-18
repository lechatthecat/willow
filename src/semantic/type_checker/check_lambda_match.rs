use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::*;
use std::collections::HashSet;

use super::*;

impl TypeChecker {
    pub(super) fn check_try_propagate(&mut self, inner: &Expr, span: Span) -> Type {
        let operand_ty = self.check_expr(inner);

        if let Type::Generic(name, args) = &operand_ty
            && name == "Option"
            && args.len() == 1
        {
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

    /// A lambda body is a new function: an enclosing loop is NOT breakable
    /// from inside it, so `loop_depth` resets for the body (willow-kzka).
    fn with_lambda_loop_boundary<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = std::mem::take(&mut self.loop_depth);
        let r = f(self);
        self.loop_depth = saved;
        r
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

    /// Reject references to enclosing-function locals inside a lambda body.
    /// Tracks names bound INSIDE the lambda (params, `let`s, loop/match/select
    /// bindings) with proper scoping; any other variable that resolves to a
    /// local of the enclosing function is a capture, which codegen cannot
    /// express yet (willow-thqe). Nested lambdas are skipped — they run their
    /// own scan when checked.
    fn check_lambda_captures(&mut self, l: &LambdaExpr) {
        let mut scopes: Vec<std::collections::HashSet<String>> =
            vec![l.params.iter().map(|p| p.name.clone()).collect()];
        match &l.body {
            LambdaBody::Expr(e) => self.scan_captures_expr(e, &mut scopes),
            LambdaBody::Block(b) => self.scan_captures_block(b, &mut scopes),
        }
    }

    fn scan_captures_block(
        &mut self,
        block: &Block,
        scopes: &mut Vec<std::collections::HashSet<String>>,
    ) {
        scopes.push(Default::default());
        for stmt in &block.stmts {
            self.scan_captures_stmt(stmt, scopes);
        }
        scopes.pop();
    }

    fn scan_captures_stmt(
        &mut self,
        stmt: &Stmt,
        scopes: &mut Vec<std::collections::HashSet<String>>,
    ) {
        match stmt {
            Stmt::Defer(d) => self.scan_captures_expr(&d.call, scopes),
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Let(l) => {
                self.scan_captures_expr(&l.init, scopes);
                scopes.last_mut().unwrap().insert(l.name.clone());
            }
            Stmt::Assign(a) => {
                self.scan_captures_expr(&a.value, scopes);
                // Writing an enclosing local is a capture too.
                self.scan_captures_name(&a.name, a.span, scopes);
            }
            Stmt::FieldAssign(f) => {
                self.scan_captures_expr(&f.object, scopes);
                self.scan_captures_expr(&f.value, scopes);
            }
            Stmt::StaticFieldAssign(s) => self.scan_captures_expr(&s.value, scopes),
            Stmt::IndexAssign(i) => {
                self.scan_captures_expr(&i.array, scopes);
                self.scan_captures_expr(&i.index, scopes);
                self.scan_captures_expr(&i.value, scopes);
            }
            Stmt::SuperInit(s) => {
                for arg in &s.args {
                    self.scan_captures_expr(&arg.expr, scopes);
                }
            }
            Stmt::If(i) => {
                self.scan_captures_expr(&i.cond, scopes);
                self.scan_captures_block(&i.then_block, scopes);
                if let Some(e) = &i.else_block {
                    self.scan_captures_block(e, scopes);
                }
            }
            Stmt::While(w) => {
                self.scan_captures_expr(&w.cond, scopes);
                self.scan_captures_block(&w.body, scopes);
            }
            Stmt::For(f) => {
                self.scan_captures_expr(&f.iterable, scopes);
                scopes.push(Default::default());
                scopes.last_mut().unwrap().insert(f.name.clone());
                self.scan_captures_block(&f.body, scopes);
                scopes.pop();
            }
            Stmt::Return(r) => {
                if let Some(v) = &r.value {
                    self.scan_captures_expr(v, scopes);
                }
            }
            Stmt::Expr(e) => self.scan_captures_expr(&e.expr, scopes),
        }
    }

    fn scan_captures_expr(
        &mut self,
        expr: &Expr,
        scopes: &mut Vec<std::collections::HashSet<String>>,
    ) {
        match expr {
            Expr::Var(name, span) => self.scan_captures_name(name, *span, scopes),
            Expr::Binary(b) => {
                self.scan_captures_expr(&b.lhs, scopes);
                self.scan_captures_expr(&b.rhs, scopes);
            }
            Expr::Unary(u) => self.scan_captures_expr(&u.expr, scopes),
            Expr::Call(c) => {
                for arg in &c.args {
                    self.scan_captures_expr(&arg.expr, scopes);
                }
            }
            Expr::MethodCall(m) => {
                self.scan_captures_expr(&m.object, scopes);
                for arg in &m.args {
                    self.scan_captures_expr(&arg.expr, scopes);
                }
            }
            Expr::StaticCall(sc) => {
                for arg in &sc.args {
                    self.scan_captures_expr(&arg.expr, scopes);
                }
            }
            Expr::FieldAccess(o, _, _) => self.scan_captures_expr(o, scopes),
            Expr::New(n) => {
                for arg in &n.args {
                    self.scan_captures_expr(&arg.expr, scopes);
                }
            }
            Expr::ObjectLiteral(o) => {
                for f in &o.fields {
                    self.scan_captures_expr(&f.value, scopes);
                }
            }
            Expr::Await(a) => self.scan_captures_expr(&a.expr, scopes),
            Expr::Print(inner, _, _) => self.scan_captures_expr(inner, scopes),
            Expr::Ternary(t) => {
                self.scan_captures_expr(&t.condition, scopes);
                self.scan_captures_expr(&t.then_expr, scopes);
                self.scan_captures_expr(&t.else_expr, scopes);
            }
            Expr::Range(r) => {
                self.scan_captures_expr(&r.start, scopes);
                self.scan_captures_expr(&r.end, scopes);
            }
            Expr::TryPropagate(inner, _) => self.scan_captures_expr(inner, scopes),
            Expr::ArrayLiteral(elements, _) => {
                for e in elements {
                    self.scan_captures_expr(e, scopes);
                }
            }
            Expr::Index(a, i, _) => {
                self.scan_captures_expr(a, scopes);
                self.scan_captures_expr(i, scopes);
            }
            Expr::Match(m) => {
                self.scan_captures_expr(&m.scrutinee, scopes);
                for arm in &m.arms {
                    scopes.push(Default::default());
                    match &arm.pattern {
                        Pattern::Binding { name, .. } => {
                            scopes.last_mut().unwrap().insert(name.clone());
                        }
                        Pattern::EnumVariantTuple { bindings, .. } => {
                            for b in bindings {
                                scopes.last_mut().unwrap().insert(b.clone());
                            }
                        }
                        Pattern::ClassDowncast { binding, .. } => {
                            scopes.last_mut().unwrap().insert(binding.clone());
                        }
                        Pattern::Wildcard(_)
                        | Pattern::LiteralBool(_, _)
                        | Pattern::LiteralInt(_, _)
                        | Pattern::EnumVariant { .. } => {}
                    }
                    match &arm.body {
                        MatchBody::Expr(e) => self.scan_captures_expr(e, scopes),
                        MatchBody::Block(b) => self.scan_captures_block(b, scopes),
                    }
                    scopes.pop();
                }
            }
            Expr::Select(s) => {
                for case in &s.cases {
                    scopes.push(Default::default());
                    match &case.kind {
                        SelectCaseKind::Recv { binding, channel } => {
                            self.scan_captures_expr(channel, scopes);
                            scopes.last_mut().unwrap().insert(binding.clone());
                        }
                        SelectCaseKind::Send { channel, value } => {
                            self.scan_captures_expr(channel, scopes);
                            self.scan_captures_expr(value, scopes);
                        }
                        SelectCaseKind::Default => {}
                    }
                    self.scan_captures_block(&case.body, scopes);
                    scopes.pop();
                }
            }
            // A nested lambda runs its own capture scan when it is checked.
            Expr::Lambda(_) => {}
            Expr::Integer(_, _)
            | Expr::Float(_, _)
            | Expr::Bool(_, _)
            | Expr::Nil(_)
            | Expr::String(_, _)
            | Expr::StaticField(_) => {}
        }
    }

    fn scan_captures_name(
        &mut self,
        name: &str,
        span: Span,
        scopes: &[std::collections::HashSet<String>],
    ) {
        if name == "self" {
            // `self` capture reads the receiver — same unsupported class.
            if self.symbols.lookup_var("self").is_some() {
                self.push_capture_error(name, span);
            }
            return;
        }
        if scopes.iter().any(|s| s.contains(name)) {
            return; // bound inside the lambda
        }
        if self.symbols.lookup_var(name).is_some() {
            self.push_capture_error(name, span);
        }
    }

    fn push_capture_error(&mut self, name: &str, span: Span) {
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E1002,
                format!("lambda cannot capture `{name}` from the enclosing function"),
            )
            .with_label(Label::primary(
                span,
                "closures are not supported yet; lambdas may only use their own parameters and locals",
            ))
            .with_help(format!(
                "pass `{name}` as a lambda parameter instead, e.g. `|{name}, ...|`"
            )),
        );
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
        self.with_lambda_loop_boundary(|this| {
            this.check_lambda_with_context_inner(l, expected_params, expected_return)
        })
    }

    fn check_lambda_with_context_inner(
        &mut self,
        l: &LambdaExpr,
        expected_params: Option<&[Type]>,
        expected_return: Option<&Type>,
    ) -> Type {
        // Lambdas are non-capturing: a body reference to an enclosing local
        // would silently read garbage in codegen, so reject it here
        // (willow-thqe). Runs before the body check, while the enclosing
        // function's locals are still the visible variable scope.
        self.check_lambda_captures(l);

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

                    self.lambda_return_stack
                        .pop()
                        .flatten()
                        .unwrap_or(Type::Void)
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
                Type::Bool if (!has_true || !has_false) => {
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
                // An arm that always returns diverges — type it `Never` so it
                // unifies with value arms (`Ok(v) => v, Err(_) => return 0`)
                // and with statement-position matches (willow-zvkv).
                if crate::semantic::type_checker::analysis::block_always_returns(block) {
                    Type::Never
                } else {
                    Type::Void
                }
            }
        }
    }
}

#[cfg(test)]
mod lambda_capture_tests {
    //! willow-thqe: lambdas are non-capturing; a body reference to an
    //! enclosing local must be rejected (it used to silently read 0).
    //! 20 perspectives: 1 read capture, 2 write capture, 3 param use ok,
    //! 4 own-let ok, 5 shadowing param ok, 6 block-scoped let ok, 7 sibling-
    //! scope leak caught, 8 capture in nested if, 9 capture in loop body,
    //! 10 capture in match arm, 11 match binding ok, 12 for-var ok,
    //! 13 nested lambda inner param ok, 14 nested lambda captures outer-lambda
    //! param caught, 15 free fn call ok (not a capture), 16 capture in call
    //! args caught, 17 capture in ternary caught, 18 self capture in method
    //! caught, 19 block-bodied lambda capture caught, 20 capture of enclosing
    //! param caught.
    use crate::diagnostics::Diagnostic;

    fn check(src: &str) -> Vec<Diagnostic> {
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude");
        checker.check_program(&program);
        checker.errors
    }

    fn ok(src: &str) {
        let d = check(src);
        assert!(d.is_empty(), "expected clean, got {d:?}");
    }

    fn capture_err(src: &str) {
        let d = check(src);
        assert!(
            d.iter().any(|d| format!("{:?}", d.code) == "E1002"),
            "expected E1002 capture error, got {d:?}"
        );
    }

    #[test]
    fn c01_read_capture_rejected() {
        capture_err("fn main() { let y = 10; let f = |x: i64| x + y; println(f(1)); }");
    }

    #[test]
    fn c02_write_capture_rejected() {
        capture_err(
            "fn main() { let mut y = 0; let f = |x: i64| { y = x; return x; }; println(f(1)); }",
        );
    }

    #[test]
    fn c03_param_use_ok() {
        ok("fn main() { let f = |x: i64| x * 2; println(f(3)); }");
    }

    #[test]
    fn c04_own_let_ok() {
        ok("fn main() { let f = |x: i64| { let d = x + 1; return d * 2; }; println(f(3)); }");
    }

    #[test]
    fn c05_shadowing_param_ok() {
        // The lambda param shadows the outer local of the same name.
        ok("fn main() { let y = 10; let f = |y: i64| y * 2; println(f(3)); }");
    }

    #[test]
    fn c06_block_scoped_let_ok() {
        ok(
            "fn main() { let f = |c: bool| { if c { let t = 1; return t; } return 0; }; println(f(true)); }",
        );
    }

    #[test]
    fn c07_sibling_scope_leak_is_capture() {
        // `t` bound in the if-block is OUT of scope afterwards; if the
        // enclosing fn has a `t`, the later use is a capture of THAT one.
        capture_err(
            "fn main() { let t = 9; let f = |c: bool| { if c { let t = 1; println(t); } return t; }; println(f(true)); }",
        );
    }

    #[test]
    fn c08_capture_in_nested_if_rejected() {
        capture_err(
            "fn main() { let y = 1; let f = |c: bool| { if c { return y; } return 0; }; println(f(true)); }",
        );
    }

    #[test]
    fn c09_capture_in_loop_body_rejected() {
        capture_err(
            "fn main() { let y = 1; let f = |n: i64| { let mut t = 0; for i in 0..n { t = t + y; } return t; }; println(f(3)); }",
        );
    }

    #[test]
    fn c10_capture_in_match_arm_rejected() {
        capture_err(
            "fn main() { let y = 1; let f = |o: Option<i64>| match o { Some(v) => v + y, None => 0, }; println(f(Some(1))); }",
        );
    }

    #[test]
    fn c11_match_binding_ok() {
        ok(
            "fn main() { let f = |o: Option<i64>| match o { Some(v) => v, None => 0, }; println(f(Some(1))); }",
        );
    }

    #[test]
    fn c12_for_var_ok() {
        ok(
            "fn main() { let f = |n: i64| { let mut t = 0; for i in 0..n { t = t + i; } return t; }; println(f(3)); }",
        );
    }

    #[test]
    fn c13_nested_lambda_inner_param_ok() {
        ok(
            "fn apply(g: fn(i64) -> i64, v: i64) -> i64 { return g(v); } \
            fn main() { let f = |x: i64| x + 1; println(apply(f, 2)); }",
        );
    }

    #[test]
    fn c14_nested_lambda_capturing_outer_lambda_param_rejected() {
        // The inner lambda captures `a`, a local (param) of the OUTER lambda.
        capture_err(
            "fn main() { let f = |a: i64| { let g = |b: i64| a + b; return g(1); }; println(f(2)); }",
        );
    }

    #[test]
    fn c15_free_fn_call_ok() {
        // Calling a free function is not a variable capture.
        ok("fn double(n: i64) -> i64 { return n * 2; } \
            fn main() { let f = |x: i64| double(x); println(f(3)); }");
    }

    #[test]
    fn c16_capture_in_call_args_rejected() {
        capture_err(
            "fn double(n: i64) -> i64 { return n * 2; } \
             fn main() { let y = 1; let f = |x: i64| double(x + y); println(f(3)); }",
        );
    }

    #[test]
    fn c17_capture_in_ternary_rejected() {
        capture_err("fn main() { let y = 1; let f = |c: bool| c ? y : 0; println(f(true)); }");
    }

    #[test]
    fn c18_self_capture_in_method_rejected() {
        capture_err(
            "class C { pub v: i64; pub fn m(self) -> i64 { let f = |x: i64| x + self.v; return f(1); } } \
             fn main() { }",
        );
    }

    #[test]
    fn c19_block_bodied_capture_rejected() {
        capture_err(
            "fn main() { let y = 2; let f = |x: i64| { let t = x * y; return t; }; println(f(3)); }",
        );
    }

    #[test]
    fn c20_enclosing_param_capture_rejected() {
        capture_err(
            "fn g(y: i64) -> i64 { let f = |x: i64| x + y; return f(1); } fn main() { println(g(5)); }",
        );
    }
}
