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
        self.check_lambda_with_context(l, None, None, false)
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

    /// Check a lambda against the callable type its context asks for. Both
    /// callable types supply the same context — parameter types and a return
    /// type — and differ only in what the lambda ends up being (willow-0g8j.2.12).
    pub(super) fn check_lambda_expecting(&mut self, l: &LambdaExpr, expected: &Type) -> Type {
        let (Type::Fn(params, ret) | Type::Closure(params, ret)) = expected else {
            return self.check_lambda(l);
        };
        if params.len() != l.params.len() {
            return self.check_lambda(l);
        }
        let actual = self.check_lambda_with_context(
            l,
            Some(params.as_slice()),
            Some(ret.as_ref()),
            matches!(expected, Type::Closure(..)),
        );
        // A `fn` value is a bare code address with no room for an environment,
        // so a capturing lambda cannot be one (willow-0g8j.2.12). Reporting it
        // here — and then answering with the EXPECTED type — keeps the message
        // about the capture rather than about a type mismatch the user cannot
        // act on.
        if matches!(expected, Type::Fn(..)) && matches!(actual, Type::Closure(..)) {
            let captured = self
                .lambda_captures
                .get(&l.span)
                .map(|c| {
                    c.iter()
                        .map(|c| format!("`{}`", c.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1011,
                    "capturing lambda cannot be used as a plain function pointer".to_string(),
                )
                .with_label(Label::primary(l.span, format!("captures {captured}")))
                .with_help(format!(
                    "a `fn` value carries no environment: take what it reads as a parameter, or, where the signature can be changed, declare it `closure({}) -> {}`",
                    params
                        .iter()
                        .map(type_name)
                        .collect::<Vec<_>>()
                        .join(", "),
                    type_name(ret)
                )),
            );
            return expected.clone();
        }
        actual
    }

    /// Work out what a lambda takes from its enclosing function
    /// (willow-0g8j.2.12). Names bound INSIDE the lambda — params, `let`s,
    /// loop/match/select bindings — are tracked with proper scoping; any other
    /// variable that resolves to a visible local is a capture.
    ///
    /// A NESTED lambda is walked too, under a scope of its own. Its free
    /// variables are captures of this lambda as well, because the environment
    /// it reads them from is the one this lambda has to carry: an inner lambda
    /// cannot reach a frame two levels up any more than it can reach one.
    ///
    /// Runs while the enclosing function's locals are still the visible
    /// variable scope, so `lookup_var` answers for exactly the names a capture
    /// could name.
    fn collect_lambda_captures(&mut self, l: &LambdaExpr) -> Vec<LambdaCapture> {
        let mut scan = CaptureScan {
            checker: self,
            scopes: vec![l.params.iter().map(|p| p.name.clone()).collect()],
            captures: Vec::new(),
            writes: Vec::new(),
        };
        match &l.body {
            LambdaBody::Expr(e) => scan.visit_expr(e),
            LambdaBody::Block(b) => scan.visit_block(b),
        }
        let CaptureScan {
            captures, writes, ..
        } = scan;
        // A capture is a COPY taken when the closure value is built
        // (willow-0g8j.2.12), so rebinding one inside the body could not change
        // anything the enclosing function goes on to observe — and a second
        // call would start from the same copy again. `let mut` does not change
        // that: sharing a variable between the two frames would need a heap
        // cell that the language does not have. Silence would be the worst of
        // the three answers, so every such write is rejected.
        for (name, span) in writes {
            if captures.iter().any(|c| c.name == name) {
                self.capture_writes.insert(span);
                self.push_capture_write(&name, span, &captures);
            }
        }
        captures
    }

    fn push_capture_write(&mut self, name: &str, span: Span, captures: &[LambdaCapture]) {
        let declaration = captures
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.declaration_span);
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E1009,
            format!("cannot assign to captured variable `{name}`"),
        )
        .with_label(Label::primary(span, "cannot assign to a capture"))
        .with_help(
            "a lambda captures by value, so this write could not be seen outside it: \
             return the new value instead, or mutate a field or element of a captured object"
                .to_string(),
        );
        if let Some(declaration) = declaration {
            diagnostic = diagnostic.with_label(Label::secondary(
                declaration,
                format!("`{name}` is declared here and captured by value"),
            ));
        }
        self.push(diagnostic);
    }

    /// `self` is the one name a lambda still cannot take: the receiver is bound
    /// by the method's own calling convention rather than by a local slot, so
    /// there is nothing for the environment to copy (willow-0g8j.2.12).
    fn push_self_capture_error(&mut self, span: Span) {
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E1002,
                "lambda cannot capture `self` from the enclosing method".to_string(),
            )
            .with_label(Label::primary(
                span,
                "`self` is not capturable; capture the fields it reads instead",
            ))
            .with_help("bind what you need first, e.g. `let n = self.count;` and capture `n`"),
        );
    }

    pub(super) fn check_lambda_with_param_context(
        &mut self,
        l: &LambdaExpr,
        expected_params: &[Type],
        expected_closure: bool,
    ) -> Type {
        if expected_params.len() != l.params.len() {
            return self.check_lambda(l);
        }
        self.check_lambda_with_context(l, Some(expected_params), None, expected_closure)
    }

    pub(super) fn check_lambda_with_context(
        &mut self,
        l: &LambdaExpr,
        expected_params: Option<&[Type]>,
        expected_return: Option<&Type>,
        expected_closure: bool,
    ) -> Type {
        // A lambda is a separate callable value. Do not attribute waits or
        // named-call edges in its body to the enclosing function (or to a lock
        // that merely constructs the lambda).
        let previous_effect_callable = self.current_effect_callable.take();
        let result = self.with_lambda_function_boundary(|this| {
            this.check_lambda_with_context_inner(
                l,
                expected_params,
                expected_return,
                expected_closure,
            )
        });
        self.current_effect_callable = previous_effect_callable;
        result
    }

    fn check_lambda_with_context_inner(
        &mut self,
        l: &LambdaExpr,
        expected_params: Option<&[Type]>,
        expected_return: Option<&Type>,
        expected_closure: bool,
    ) -> Type {
        // What the lambda takes from the enclosing function decides which of
        // the two callable types it has, so this runs first — and it must run
        // while the enclosing function's locals are still the visible variable
        // scope (willow-0g8j.2.12).
        let captures = self.collect_lambda_captures(l);

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

        // A lambda that takes nothing is a plain code address; one that
        // captures is a GC environment object and is CALLED differently, so the
        // two carry different types (willow-0g8j.2.12). A non-capturing lambda
        // still becomes a closure when the context asks for one, so that
        // `closure(i64) -> i64` accepts `|x| x * 2` — the environment is then
        // just the code pointer.
        let fn_ty = if captures.is_empty() && !expected_closure {
            Type::Fn(param_types, Box::new(ret_ty))
        } else {
            Type::Closure(param_types, Box::new(ret_ty))
        };
        self.lambda_captures.insert(l.span, captures);
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
/// One value a lambda takes from its enclosing function (willow-0g8j.2.12).
///
/// The name is the enclosing function's spelling, which is also the name the
/// lifted body binds the environment slot under, so the body's own references
/// resolve to it without renaming.
#[derive(Debug, Clone, PartialEq)]
pub struct LambdaCapture {
    pub name: String,
    pub ty: Type,
    /// Where it was declared, for the diagnostic that points at it.
    pub declaration_span: Span,
}

struct CaptureScan<'a> {
    checker: &'a mut TypeChecker,
    scopes: Vec<HashSet<String>>,
    /// The captures found so far, in first-mention order — which is the order
    /// the closure environment lays its slots out.
    captures: Vec<LambdaCapture>,
    /// Names the lambda ASSIGNS to that are not bound inside it. Kept apart
    /// from `captures` because whether a write is legal depends on the
    /// declaration, which is decided once the whole body has been seen.
    writes: Vec<(String, Span)>,
}

impl CaptureScan<'_> {
    fn bound_inside(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    fn note_use(&mut self, name: &str, span: Span) {
        if name == "self" {
            // The receiver has no local slot to copy from.
            if !self.bound_inside("self") && self.checker.symbols.lookup_var("self").is_some() {
                self.checker.push_self_capture_error(span);
            }
            return;
        }
        if self.bound_inside(name) {
            return; // bound inside the lambda
        }
        if self.captures.iter().any(|c| c.name == name) {
            return; // already recorded; first mention fixes the slot
        }
        let Some(info) = self.checker.symbols.lookup_var(name).cloned() else {
            return; // a free function, a class, a constant — not a capture
        };
        self.captures.push(LambdaCapture {
            name: name.to_string(),
            ty: info.ty,
            declaration_span: info.declaration_span,
        });
    }

    fn note_write(&mut self, name: &str, span: Span) {
        if self.bound_inside(name) {
            return;
        }
        if self.checker.symbols.lookup_var(name).is_some() {
            self.writes.push((name.to_string(), span));
        }
    }
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

    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
        // Writing an enclosing local captures it too, and an assignment target
        // is a name on the statement rather than a `Var` the walk can reach.
        if let Stmt::Assign(assign) = stmt {
            self.note_use(&assign.name, assign.span);
            self.note_write(&assign.name, assign.span);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Var(name, span) => self.note_use(name, *span),
            // Calling a local function VALUE captures it: the callee is a name
            // on the call node, not a `Var` the walk reaches, exactly as an
            // assignment target is. A free function's name resolves to no
            // variable, so `note_use` ignores it.
            Expr::Call(call) => self.note_use(&call.callee, call.span),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod lambda_capture_tests {
    //! What a lambda takes from its enclosing function, and what that makes it.
    //!
    //! willow-thqe first wrote these perspectives when a capture was an error
    //! (it used to silently read 0). willow-0g8j.2.12 gave lambdas a real
    //! environment, so the same 30 shapes now pin the OPPOSITE answer: the
    //! capture is accepted, recorded in slot order, and turns the lambda into a
    //! `closure` value. The scan itself is the same code, so a container the
    //! shared walk (willow-uqzx.1.1) misses is still a silent wrong answer —
    //! now a capture the environment never carries rather than a missed error.
    //!
    //! 1 read capture, 2 param use, 3 own `let`, 4 shadowing param, 5 block-
    //! scoped `let`, 6 sibling-scope leak, 7 nested `if`, 8 loop body, 9 match
    //! arm, 10 match binding, 11 `for` variable, 12 nested-lambda inner param,
    //! 13 nested lambda captures the outer lambda's param, 14 free fn call,
    //! 15 call argument, 16 ternary, 17 block body, 18 enclosing param,
    //! 19 index expression, 20 range bound, 21 `defer` body, 22 `new`
    //! argument, 23 array-literal element, 24 index-assignment target,
    //! 25 `for` iterable, 26 `while` condition, 27 a `let` initializer is not
    //! excused by its own binding, 28 match payload binding.
    //!
    //! Then the rules the environment brought with it (willow-0g8j.2.12):
    //! 29 `self` is still E1002, 30 a write to a capture is E1009, 31 `let mut`
    //! does not excuse it, 32 the write is reported once, 33 a capturing lambda
    //! is not a `fn` value (E1011), 34 but is a `closure` one, 35 a
    //! non-capturing lambda stays a `fn` value, 36 calling a captured function
    //! value captures it, 37 repeated mentions share one slot, 38 slots follow
    //! first-mention order, 39 a field write THROUGH a capture is not a write
    //! to it, 40 a capture two frames up rides both environments.
    use crate::diagnostics::Diagnostic;

    /// Check `src`, and report both its diagnostics and what each lambda
    /// captures — outer lambdas first, and within one lambda in the order the
    /// closure environment lays its slots out.
    fn scan(src: &str) -> (Vec<Diagnostic>, Vec<Vec<String>>) {
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude");
        checker.check_program(&program);
        let mut lambdas: Vec<_> = checker.lambda_captures.iter().collect();
        lambdas.sort_by_key(|(span, _)| (span.start, span.end));
        let captures = lambdas
            .into_iter()
            .map(|(_, c)| c.iter().map(|c| c.name.clone()).collect())
            .collect();
        (checker.errors, captures)
    }

    fn check(src: &str) -> Vec<Diagnostic> {
        scan(src).0
    }

    fn ok(src: &str) {
        let d = check(src);
        assert!(d.is_empty(), "expected clean, got {d:?}");
    }

    /// `src` checks clean and its lambdas capture exactly `expected`, one row
    /// per lambda in source order.
    fn captures(src: &str, expected: &[&[&str]]) {
        let (diagnostics, actual) = scan(src);
        assert!(
            diagnostics.is_empty(),
            "expected clean, got {diagnostics:?}"
        );
        let expected: Vec<Vec<String>> = expected
            .iter()
            .map(|l| l.iter().map(|n| (*n).to_string()).collect())
            .collect();
        assert_eq!(actual, expected, "capture slots differ");
    }

    /// The one lambda in `src` captures exactly `names`.
    fn captures_one(src: &str, names: &[&str]) {
        captures(src, &[names]);
    }

    fn no_capture(src: &str) {
        captures(src, &[&[]]);
    }

    fn err(src: &str, code: &str) {
        let d = check(src);
        assert!(
            d.iter().any(|d| format!("{:?}", d.code) == code),
            "expected {code}, got {d:?}"
        );
    }

    /// `src` reports exactly one diagnostic, with code `code`. Captures reach
    /// the checker twice — once in the scan, once in the body check — so a rule
    /// written in the wrong place reads as a duplicate rather than a miss.
    fn only_err(src: &str, code: &str) {
        let d = check(src);
        assert_eq!(d.len(), 1, "expected exactly one diagnostic, got {d:?}");
        assert_eq!(format!("{:?}", d[0].code), code, "got {d:?}");
    }

    #[test]
    fn c01_read_capture_is_recorded() {
        captures_one(
            "fn main() { let y = 10; let f = |x: i64| x + y; println(f(1)); }",
            &["y"],
        );
    }

    #[test]
    fn c02_param_use_is_not_a_capture() {
        no_capture("fn main() { let f = |x: i64| x * 2; println(f(3)); }");
    }

    #[test]
    fn c03_own_let_is_not_a_capture() {
        no_capture(
            "fn main() { let f = |x: i64| { let d = x + 1; return d * 2; }; println(f(3)); }",
        );
    }

    #[test]
    fn c04_shadowing_param_is_not_a_capture() {
        // The lambda param shadows the outer local of the same name.
        no_capture("fn main() { let y = 10; let f = |y: i64| y * 2; println(f(3)); }");
    }

    #[test]
    fn c05_block_scoped_let_is_not_a_capture() {
        no_capture(
            "fn main() { let f = |c: bool| { if c { let t = 1; return t; } return 0; }; println(f(true)); }",
        );
    }

    #[test]
    fn c06_sibling_scope_leak_is_a_capture() {
        // `t` bound in the if-block is OUT of scope afterwards; the later use
        // reads the enclosing fn's `t`, so the environment must carry it.
        captures_one(
            "fn main() { let t = 9; let f = |c: bool| { if c { let t = 1; println(t); } return t; }; println(f(true)); }",
            &["t"],
        );
    }

    #[test]
    fn c07_capture_in_nested_if() {
        captures_one(
            "fn main() { let y = 1; let f = |c: bool| { if c { return y; } return 0; }; println(f(true)); }",
            &["y"],
        );
    }

    #[test]
    fn c08_capture_in_loop_body() {
        captures_one(
            "fn main() { let y = 1; let f = |n: i64| { let mut t = 0; for i in 0..n { t = t + y; } return t; }; println(f(3)); }",
            &["y"],
        );
    }

    #[test]
    fn c09_capture_in_match_arm() {
        captures_one(
            "fn main() { let y = 1; let f = |o: Option<i64>| match o { Some(v) => v + y, None => 0, }; println(f(Some(1))); }",
            &["y"],
        );
    }

    #[test]
    fn c10_match_binding_is_not_a_capture() {
        no_capture(
            "fn main() { let f = |o: Option<i64>| match o { Some(v) => v, None => 0, }; println(f(Some(1))); }",
        );
    }

    #[test]
    fn c11_for_var_is_not_a_capture() {
        no_capture(
            "fn main() { let f = |n: i64| { let mut t = 0; for i in 0..n { t = t + i; } return t; }; println(f(3)); }",
        );
    }

    #[test]
    fn c12_nested_lambda_inner_param_is_not_a_capture() {
        no_capture(
            "fn apply(g: fn(i64) -> i64, v: i64) -> i64 { return g(v); } \
            fn main() { let f = |x: i64| x + 1; println(apply(f, 2)); }",
        );
    }

    #[test]
    fn c13_nested_lambda_capturing_outer_lambda_param() {
        // The inner lambda reads `a`, a local (param) of the OUTER lambda, so
        // the outer one has nothing to capture and the inner one has `a`.
        captures(
            "fn main() { let f = |a: i64| { let g = |b: i64| a + b; return g(1); }; println(f(2)); }",
            &[&[], &["a"]],
        );
    }

    #[test]
    fn c14_free_fn_call_is_not_a_capture() {
        // A free function's name resolves to no variable, so there is nothing
        // to put in an environment.
        no_capture(
            "fn double(n: i64) -> i64 { return n * 2; } \
            fn main() { let f = |x: i64| double(x); println(f(3)); }",
        );
    }

    #[test]
    fn c15_capture_in_call_args() {
        captures_one(
            "fn double(n: i64) -> i64 { return n * 2; } \
             fn main() { let y = 1; let f = |x: i64| double(x + y); println(f(3)); }",
            &["y"],
        );
    }

    #[test]
    fn c16_capture_in_ternary() {
        captures_one(
            "fn main() { let y = 1; let f = |c: bool| c ? y : 0; println(f(true)); }",
            &["y"],
        );
    }

    #[test]
    fn c17_block_bodied_capture() {
        captures_one(
            "fn main() { let y = 2; let f = |x: i64| { let t = x * y; return t; }; println(f(3)); }",
            &["y"],
        );
    }

    #[test]
    fn c18_enclosing_param_capture() {
        captures_one(
            "fn g(y: i64) -> i64 { let f = |x: i64| x + y; return f(1); } fn main() { println(g(5)); }",
            &["y"],
        );
    }

    // --- Shared structural AST walk (willow-uqzx.1.1) ---
    //
    // The scan reads the shared walk in `parser::visit`, so a slot the walk
    // misses is a value the environment never carries — the body would read an
    // empty slot at runtime — and a binding the walk forgets is a capture of a
    // name the enclosing frame does not even have. These perspectives pin the
    // containers the original hand-written scan had to remember.

    #[test]
    fn c19_capture_in_an_index_expression() {
        captures_one(
            "import std::collections::Array; \
             fn main() { let i = 1; let f = |xs: Array<i64>| xs[i]; println(f([1, 2])); }",
            &["i"],
        );
    }

    #[test]
    fn c20_capture_in_a_range_bound() {
        captures_one(
            "fn main() { let hi = 3; let f = |lo: i64| { let mut t = 0; for k in lo..hi { t = t + k; } return t; }; println(f(0)); }",
            &["hi"],
        );
    }

    #[test]
    fn c21_capture_in_a_defer_body() {
        captures_one(
            "fn main() { let y = 1; let f = |x: i64| { defer { println(y); } return x; }; println(f(1)); }",
            &["y"],
        );
    }

    #[test]
    fn c22_capture_in_a_new_argument() {
        captures_one(
            "class Cell { pub value: i64; } \
             fn main() { let y = 1; let f = |x: i64| { let c = new Cell(y); return x; }; println(f(1)); }",
            &["y"],
        );
    }

    #[test]
    fn c23_capture_in_an_array_literal_element() {
        captures_one(
            "import std::collections::Array; \
             fn main() { let y = 1; let f = |x: i64| { let xs: Array<i64> = [y]; return x; }; println(f(1)); }",
            &["y"],
        );
    }

    #[test]
    fn c24_capture_as_an_index_assignment_target() {
        // Writing an ELEMENT of a captured array is a read of the array: the
        // capture is the reference, and the write lands on the same object the
        // enclosing function holds.
        captures_one(
            "import std::collections::Array; \
             fn main() { let xs: Array<i64> = [0]; let f = |x: i64| { xs[0] = x; return x; }; println(f(1)); }",
            &["xs"],
        );
    }

    /// The iterable of a `for` inside the lambda is evaluated in the lambda,
    /// so naming an enclosing local there is a capture.
    #[test]
    fn c25_capture_in_a_for_iterable() {
        captures_one(
            "import std::collections::Array; \
             fn main() { let xs: Array<i64> = [1]; let f = |x: i64| { for k in xs { println(k); } return x; }; println(f(1)); }",
            &["xs"],
        );
    }

    #[test]
    fn c26_capture_in_a_while_condition() {
        captures_one(
            "fn main() { let limit = 3; let f = |x: i64| { let mut i = 0; while i < limit { i = i + 1; } return x; }; println(f(1)); }",
            &["limit"],
        );
    }

    /// A `let` binds after its own initializer, so the initializer still names
    /// the enclosing local — the shadowing does not retroactively excuse it.
    #[test]
    fn c27_let_initializer_capture_is_not_excused_by_the_binding() {
        captures_one(
            "fn main() { let y = 1; let f = |x: i64| { let y = y + x; return y; }; println(f(1)); }",
            &["y"],
        );
    }

    /// The enum payload binding of a match arm is bound by the arm, so using
    /// it is not a capture (the walk binds pattern names).
    #[test]
    fn c28_match_payload_binding_is_not_a_capture() {
        no_capture(
            "fn main() { let f = |o: Option<i64>| match o { Some(v) => v, None => 0 }; println(f(Some(1))); }",
        );
    }

    // --- What an environment does and does not allow (willow-0g8j.2.12) ---

    /// `self` is bound by the method's calling convention, not by a local slot,
    /// so there is nothing for the environment to copy.
    #[test]
    fn c29_self_capture_in_a_method_is_rejected() {
        err(
            "class C { pub v: i64; pub fn m(self) -> i64 { let f = |x: i64| x + self.v; return f(1); } } \
             fn main() { }",
            "E1002",
        );
    }

    /// A capture is a copy, so a write inside the body could never be seen
    /// outside it.
    #[test]
    fn c30_write_to_a_capture_is_rejected() {
        err(
            "fn main() { let mut y = 0; let f = |x: i64| { y = x; return x; }; println(f(1)); }",
            "E1009",
        );
    }

    /// `let mut` on the enclosing binding does not make the copy shared, so it
    /// does not excuse the write either.
    #[test]
    fn c31_let_mut_does_not_excuse_a_capture_write() {
        err(
            "fn main() { let mut n = 1; let f = |x: i64| { n = n + x; return n; }; println(f(2)); }",
            "E1009",
        );
    }

    /// The body is checked again after the scan, where the target still
    /// resolves to the enclosing function's immutable local. Only the capture
    /// error should come out: the mutability one points at the wrong fix.
    #[test]
    fn c32_a_capture_write_is_reported_once() {
        only_err(
            "fn main() { let y = 0; let f = |x: i64| { y = x; return x; }; println(f(1)); }",
            "E1009",
        );
    }

    /// A `fn` value is a bare code address with no room for an environment.
    #[test]
    fn c33_a_capturing_lambda_is_not_a_fn_value() {
        err(
            "fn apply(g: fn(i64) -> i64, v: i64) -> i64 { return g(v); } \
             fn main() { let y = 1; println(apply(|x: i64| x + y, 2)); }",
            "E1011",
        );
    }

    /// The same lambda against a `closure` parameter is exactly what the
    /// environment is for.
    #[test]
    fn c34_a_capturing_lambda_is_a_closure_value() {
        captures_one(
            "fn apply(g: closure(i64) -> i64, v: i64) -> i64 { return g(v); } \
             fn main() { let y = 1; println(apply(|x: i64| x + y, 2)); }",
            &["y"],
        );
    }

    /// Nothing captured means nothing to carry, so the lambda stays a plain
    /// `fn` value and a `fn` parameter still takes it.
    #[test]
    fn c35_a_non_capturing_lambda_stays_a_fn_value() {
        no_capture(
            "fn apply(g: fn(i64) -> i64, v: i64) -> i64 { return g(v); } \
             fn main() { println(apply(|x: i64| x + 1, 2)); }",
        );
    }

    /// The callee of a call is a name on the call node rather than a `Var` the
    /// walk reaches, so calling a captured function value has to be noted by
    /// hand — otherwise the environment would not carry the thing being called.
    #[test]
    fn c36_calling_a_captured_function_value_captures_it() {
        captures(
            "fn main() { let g = |x: i64| x * 2; let f = |x: i64| g(x) + 1; println(f(3)); }",
            &[&[], &["g"]],
        );
    }

    /// One slot per captured variable, however many times the body mentions it.
    #[test]
    fn c37_repeated_mentions_share_one_slot() {
        captures_one(
            "fn main() { let y = 2; let f = |x: i64| x + y * y + y; println(f(1)); }",
            &["y"],
        );
    }

    /// Slot order is first-mention order, which is what the environment layout
    /// and the lifted body's parameter binding both read.
    #[test]
    fn c38_slots_follow_first_mention_order() {
        captures_one(
            "fn main() { let a = 1; let b = 2; let f = |x: i64| b + a + x + b; println(f(0)); }",
            &["b", "a"],
        );
    }

    /// A capture holds the same reference the enclosing function holds, so
    /// writing a FIELD through it is an ordinary mutation, not a rebinding of
    /// the capture.
    #[test]
    fn c39_a_field_write_through_a_capture_is_allowed() {
        captures_one(
            "class Cell { pub value: i64; } \
             fn main() { let c = new Cell(1); let f = |x: i64| { c.value = x; return x; }; println(f(2)); println(c.value); }",
            &["c"],
        );
    }

    /// An inner lambda cannot reach a frame two levels up, so the value rides
    /// BOTH environments: the outer lambda captures it to hand it on.
    #[test]
    fn c40_a_capture_two_frames_up_rides_both_environments() {
        captures(
            "fn main() { let n = 5; let f = |x: i64| { let g = |y: i64| y + n; return g(x); }; println(f(1)); }",
            &[&["n"], &["n"]],
        );
    }

    #[test]
    fn c41_a_capture_free_lambda_reports_no_slots() {
        ok("fn main() { let f = |x: i64| x; println(f(1)); }");
    }
}
