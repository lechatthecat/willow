use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::visit::{AstVisitor, walk_expr, walk_stmt};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::symbols::*;
use std::collections::HashSet;

use super::*;

impl TypeChecker {
    pub(super) fn check_try_propagate(&mut self, inner: &Expr, span: Span) -> Type {
        let operand_ty = self.check_expr(inner);

        if let Some(some_ty) = builtin_types::unary_arg(&operand_ty, B::Option).cloned() {
            let return_ty = self.current_return_type.clone();
            if builtin_types::unary_arg(&return_ty, B::Option).is_some() {
                return some_ty;
            } else {
                let other = &return_ty;
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

        // Otherwise the operand must be Result<T,E>.
        let (ok_ty, err_ty) = match builtin_types::binary_args(&operand_ty, B::Result) {
            Some((ok, err)) => (ok.clone(), err.clone()),
            None => {
                let other = &operand_ty;
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
        match builtin_types::binary_args(&return_ty, B::Result) {
            Some((_, return_err)) => {
                if *return_err == err_ty || *return_err == Type::Void || err_ty == Type::Void {
                    // ok_ty is the success value type
                    ok_ty
                } else if self.err_converts_via_into(&err_ty, return_err) {
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
                                type_name(return_err),
                                type_name(&err_ty)
                            ),
                        )
                        .with_label(Label::primary(span, "error type mismatch"))
                        .with_help(format!(
                            "implement `Into<{}>` on `{}` to allow `?` to convert the error",
                            type_name(return_err),
                            type_name(&err_ty)
                        )),
                    );
                    ok_ty
                }
            }
            None => {
                let other = &return_ty;
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

    /// A lambda body is a new function, and every piece of state that describes
    /// "where in the current function am I" has to stop at that boundary.
    ///
    /// * `loop_depth`: an enclosing loop is NOT breakable from inside the body
    ///   (willow-kzka).
    /// * `lock_depth`: the lambda is only CONSTRUCTED inside a critical
    ///   section. Its body runs whenever it is called, holding nothing, so a
    ///   `lock` there is not a nested acquisition (willow-3kty).
    /// * `current_async_context`: a lambda has no `async` form in the grammar,
    ///   and the backend lifts its body into a plain private function. Leaving
    ///   the enclosing `async fn`'s context switched on told every check in
    ///   here that the body had a task frame to suspend into, so `lock` passed
    ///   E2603 and `await` passed E0801 and both reached codegen, where one
    ///   ICEd on an unbound binding and the other on an unsplit await
    ///   (willow-3kty).
    ///
    /// Restoring rather than clearing matters: the lambda is an expression of
    /// the enclosing body, which continues after it.
    fn with_lambda_function_boundary<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved_loop = std::mem::take(&mut self.loop_depth);
        let saved_lock = std::mem::take(&mut self.lock_depth);
        let saved_async = std::mem::replace(&mut self.current_async_context, false);
        let r = f(self);
        self.loop_depth = saved_loop;
        self.lock_depth = saved_lock;
        self.current_async_context = saved_async;
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
        let mut scan = CaptureScan {
            checker: self,
            scopes: vec![l.params.iter().map(|p| p.name.clone()).collect()],
        };
        match &l.body {
            LambdaBody::Expr(e) => scan.visit_expr(e),
            LambdaBody::Block(b) => scan.visit_block(b),
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
        // A lambda is a separate callable value. Do not attribute waits or
        // named-call edges in its body to the enclosing function (or to a lock
        // that merely constructs the lambda).
        let previous_effect_callable = self.current_effect_callable.take();
        let result = self.with_lambda_function_boundary(|this| {
            this.check_lambda_with_context_inner(l, expected_params, expected_return)
        });
        self.current_effect_callable = previous_effect_callable;
        result
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
        // An annotation is normalized the way a named function's is: it may
        // spell an item-imported enum by the local name (`Rank`, under
        // `import signal::Level as Rank;`), and only the identity is comparable
        // to what the rest of the program produces (willow-0g8j.3).
        let mut param_types = Vec::new();
        for (idx, p) in l.params.iter().enumerate() {
            match &p.ty {
                Some(ty) => {
                    self.validate_type(ty, p.span);
                    let ty = self.normalize_type(ty, p.span);
                    param_types.push(ty);
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
        let annotated_return = l
            .return_type
            .as_ref()
            .map(|ty| self.normalize_type(ty, l.span));
        let expected_ret = annotated_return.as_ref().or(expected_return);
        if let Some(ret) = expected_ret {
            self.validate_type(ret, l.span);
        }

        // Type-check the body with params in scope.
        self.symbols.push_scope();
        for (p, ty) in l.params.iter().zip(&param_types) {
            self.define_var(
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
        let saved_block_depth = self.lexical_block_depth;
        self.lexical_block_depth = 0;

        let body_ty = match &l.body {
            LambdaBody::Expr(e) => self.check_expr(e),
            LambdaBody::Block(b) => {
                if let Some(ann) = expected_ret {
                    // Annotation provided: validate return stmts against it.
                    self.current_return_type = ann.clone();
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    // A block-bodied lambda returns only through `return`, so
                    // one that owes a value has the same obligation a named
                    // function has (willow-x8sj).
                    let annotation = ann.clone();
                    self.check_all_paths_return(
                        b,
                        &annotation,
                        l.span,
                        ReturnSite::Lambda { inferred: false },
                    );
                    annotation
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
                    // The obligation is the same one an annotated lambda has;
                    // only the moment differs. The type is the RESULT of the
                    // walk above, so the check has to run here rather than
                    // before the body, and it is skipped for the inferred
                    // `void` that a lambda returning nothing lands on
                    // (willow-x8sj).
                    self.check_all_paths_return(
                        b,
                        &inferred,
                        l.span,
                        ReturnSite::Lambda { inferred: true },
                    );
                    inferred
                }
            }
        };
        self.lexical_block_depth = saved_block_depth;
        self.current_return_type = saved_ret_ty;
        self.symbols.pop_scope();

        let ret_ty = match &annotated_return {
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

        let fn_ty = Type::Fn(param_types, Box::new(ret_ty));
        // Record the lambda's type here rather than leaving it to `check_expr`:
        // a lambda passed as a call argument is checked through
        // `check_fn_arg_with_param_context`, which never goes through
        // `check_expr`, and that is exactly the case whose parameter types come
        // from the call site rather than from annotations. Without this the
        // backend and the HIR lowering would have no type for it at all
        // (willow-0g8j.3, replacing the `lambda_fn_types` side table).
        self.expr_types.insert(l.span, fn_ty.clone());
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
        // A bare `Boxy(v)` / `Round` is only the enum's variant for a unit that
        // can name the enum bare; an importer of `kinds` writes
        // `kinds::Kind::Boxy(v)` (willow-itcw). The qualified arms below stay
        // open to every spelling the unit can actually resolve.
        let bare = self.enum_nameable_bare(&info.name);
        match pattern {
            Pattern::ClassDowncast {
                class_name,
                binding,
                span,
            } => {
                let variant = info.variants.iter().find(|v| v.name == *class_name)?;
                if !bare || variant.payload_types.is_empty() {
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
                if !bare || !variant.payload_types.is_empty() {
                    return None;
                }
                Some(Pattern::EnumVariant {
                    enum_name: enum_name.clone(),
                    variant: name.clone(),
                    span: *span,
                })
            }
            // The same enum under another of its spellings: a module matching
            // on its own `Level` against a scrutinee typed `signal::Level`, or
            // an importer matching `Level::On` on one it imported by item name.
            // Rewriting to the scrutinee's spelling is what makes one enum one
            // type however the unit reached it (willow-itcw), and the resolution
            // is recorded, so the back end reads the canonical name too.
            Pattern::EnumVariant {
                enum_name: written,
                variant,
                span,
            } if written != enum_name && self.canonical_type_name(written) == info.name => {
                Some(Pattern::EnumVariant {
                    enum_name: enum_name.clone(),
                    variant: variant.clone(),
                    span: *span,
                })
            }
            Pattern::EnumVariantTuple {
                enum_name: written,
                variant,
                bindings,
                span,
            } if written != enum_name && self.canonical_type_name(written) == info.name => {
                Some(Pattern::EnumVariantTuple {
                    enum_name: enum_name.clone(),
                    variant: variant.clone(),
                    bindings: bindings.clone(),
                    span: *span,
                })
            }
            _ => None,
        }
    }

    pub(super) fn check_match_expr(&mut self, m: &MatchExpr) -> Type {
        // The type this `match` flows into, when it was reached through
        // `check_expr_expecting` (willow-0g8j.3). Taken, not borrowed: a
        // `match` nested anywhere inside an arm gets its own context or none,
        // never this one by accident.
        let expected = self.match_expected.take();
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
                    self.define_var(
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
                self.define_var(
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
                self.define_var(
                    name.clone(),
                    VarInfo {
                        ty: scrutinee_ty.clone(),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            let arm_ty = self.check_match_body(&arm.body, expected.as_ref());
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

    pub(super) fn check_match_body(&mut self, body: &MatchBody, expected: Option<&Type>) -> Type {
        self.check_match_body_inner(body, expected)
    }

    fn check_match_body_inner(&mut self, body: &MatchBody, expected: Option<&Type>) -> Type {
        match body {
            MatchBody::Expr(expr) => {
                // An arm VALUE is checked against the type the whole `match`
                // flows into, so `return match x { Some(v) => Result::Ok(v),
                // None => Result::Err("none") }` types both arms as the
                // function's `Result<i64, String>` instead of leaving the first
                // one an `Ok` with no error type (willow-0g8j.3). This is what
                // a ternary in the same position already does.
                match expected {
                    Some(expected) => {
                        let expected = expected.clone();
                        self.check_expr_expecting(expr, &expected)
                    }
                    None => self.check_expr(expr),
                }
            }
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

/// The lambda capture scan, on the shared structural walk (willow-uqzx.1.1).
///
/// It needs exactly the walk's own shadowing rule — a name bound inside the
/// lambda is not a capture — so the scope stack comes from the walker's
/// [`AstVisitor::enter_scope`]/[`AstVisitor::bind`] hooks rather than being
/// re-implemented here. Everything else is one check on [`Expr::Var`].
struct CaptureScan<'a> {
    checker: &'a mut TypeChecker,
    scopes: Vec<HashSet<String>>,
}

impl AstVisitor for CaptureScan<'_> {
    fn enter_scope(&mut self) {
        self.scopes.push(HashSet::new());
    }

    fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &str) {
        self.scopes
            .last_mut()
            .expect("capture scan scope")
            .insert(name.to_string());
    }

    /// A nested lambda runs its own capture scan when it is checked.
    fn visit_lambda(&mut self, _lambda: &LambdaExpr) {}

    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
        // Writing an enclosing local captures it too, and an assignment target
        // is a name on the statement rather than a `Var` the walk can reach.
        if let Stmt::Assign(assign) = stmt {
            self.checker
                .scan_captures_name(&assign.name, assign.span, &self.scopes);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Var(name, span) = expr {
            self.checker.scan_captures_name(name, *span, &self.scopes);
        }
        walk_expr(self, expr);
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
    //!
    //! willow-uqzx.1.1 moved the scan onto the shared structural walk and added
    //! 21-30: 21 index expression, 22 range bound, 23 defer body, 24 `new`
    //! argument, 25 array-literal element, 26 index-assignment target, 27 `for`
    //! iterable, 28 `while` condition, 29 a `let` initializer is not excused by
    //! its own binding, 30 a match payload binding is not a capture.
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

    // --- Shared structural AST walk (willow-uqzx.1.1) ---
    //
    // The scan reads the shared walk in `parser::visit`, so a slot the walk
    // misses is a capture that silently reads 0 at runtime, and a binding the
    // walk forgets is a false rejection of correct code. These perspectives
    // pin the containers the original hand-written scan had to remember.

    #[test]
    fn c21_capture_in_an_index_expression_rejected() {
        capture_err(
            "import std::collections::Array; \
             fn main() { let i = 1; let f = |xs: Array<i64>| xs[i]; println(f([1, 2])); }",
        );
    }

    #[test]
    fn c22_capture_in_a_range_bound_rejected() {
        capture_err(
            "fn main() { let hi = 3; let f = |lo: i64| { let mut t = 0; for k in lo..hi { t = t + k; } return t; }; println(f(0)); }",
        );
    }

    #[test]
    fn c23_capture_in_a_defer_body_rejected() {
        capture_err(
            "fn main() { let y = 1; let f = |x: i64| { defer { println(y); } return x; }; println(f(1)); }",
        );
    }

    #[test]
    fn c24_capture_in_a_new_argument_rejected() {
        capture_err(
            "class Cell { pub value: i64; } \
             fn main() { let y = 1; let f = |x: i64| { let c = new Cell(y); return x; }; println(f(1)); }",
        );
    }

    #[test]
    fn c25_capture_in_an_array_literal_element_rejected() {
        capture_err(
            "import std::collections::Array; \
             fn main() { let y = 1; let f = |x: i64| { let xs: Array<i64> = [y]; return x; }; println(f(1)); }",
        );
    }

    #[test]
    fn c26_capture_as_an_index_assignment_target_rejected() {
        capture_err(
            "import std::collections::Array; \
             fn main() { let xs: Array<i64> = [0]; let f = |x: i64| { xs[0] = x; return x; }; println(f(1)); }",
        );
    }

    /// The iterable of a `for` inside the lambda is evaluated in the lambda,
    /// so naming an enclosing local there is a capture.
    #[test]
    fn c27_capture_in_a_for_iterable_rejected() {
        capture_err(
            "import std::collections::Array; \
             fn main() { let xs: Array<i64> = [1]; let f = |x: i64| { for k in xs { println(k); } return x; }; println(f(1)); }",
        );
    }

    #[test]
    fn c28_capture_in_a_while_condition_rejected() {
        capture_err(
            "fn main() { let limit = 3; let f = |x: i64| { let mut i = 0; while i < limit { i = i + 1; } return x; }; println(f(1)); }",
        );
    }

    /// A `let` binds after its own initializer, so the initializer still names
    /// the enclosing local — the shadowing does not retroactively excuse it.
    #[test]
    fn c29_let_initializer_capture_is_not_excused_by_the_binding() {
        capture_err(
            "fn main() { let y = 1; let f = |x: i64| { let y = y + x; return y; }; println(f(1)); }",
        );
    }

    /// The enum payload binding of a match arm is bound by the arm, so using
    /// it is not a capture (the walk binds pattern names).
    #[test]
    fn c30_match_payload_binding_is_not_a_capture() {
        ok(
            "fn main() { let f = |o: Option<i64>| match o { Some(v) => v, None => 0 }; println(f(Some(1))); }",
        );
    }
}
