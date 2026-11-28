use super::*;

#[willow_continuations::checker]
impl TypeChecker {
    pub(in crate::semantic::type_checker) fn check_block(&mut self, block: &Block) {
        self.check_block_scope(block, true);
    }

    pub(in crate::semantic::type_checker) fn check_block_scope(
        &mut self,
        block: &Block,
        lexical: bool,
    ) {
        let capture_errors_start = self.errors.len();
        self.local.lexical_block_depth += u32::from(lexical);
        self.symbols.push_scope();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        for stmt in &block.stmts {
            if let Stmt::Let(binding) = stmt
                && let Some((uses, _)) = self.local.inferred_maps.remove(&binding.span)
            {
                for id in uses {
                    self.local.pending_map_uses.remove(&id);
                }
                self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                    "cannot infer map key and value types")
                    .with_label(Label::primary(binding.span, "map needs type information"))
                    .with_help("add a Map<K, V> annotation or insert a key and value before using its contents"));
            }
        }
        self.symbols.pop_scope();
        self.local.lexical_block_depth -= u32::from(lexical);
        if self.local.lexical_block_depth == 0 {
            self.check_transfer_ownership(crate::parser::iter::AstEvent::Block(block));
            self.refine_capture_flow(
                crate::parser::iter::AstEvent::Block(block),
                capture_errors_start,
            );
        }
        self.end_capture_scope(block);
    }

    /// Type-check `lock <target> as [mut] <binding> { .. }`.
    ///
    /// V1 rules, in the order they are reported:
    ///   * the target must be the lock type the mode requires (E2602),
    ///   * the statement is legal only inside an `async fn` (E2603),
    ///   * no user-visible suspension inside the critical section (E2604),
    ///   * nested acquisitions must be provably distinct and ordered (E2605).
    ///
    /// A statement that breaks none of those lowers through
    /// `emit_coop_lock`. Only `lock read` / `lock write` on an
    /// `RwLock<T>` remain behind the staged gate (E2502) until Stage 5.
    pub(in crate::semantic::type_checker) fn check_lock_stmt(&mut self, s: &LockStmt) {
        if s.walk
            && (s.mode != LockMode::Mutex
                || !s.additional.is_empty()
                || !matches!(&s.target, Expr::Var(name, ..) if name != &s.binding))
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2601,
                    "lock walk requires one Mutex cursor variable and a distinct value binding",
                )
                .with_label(Label::primary(
                    s.header_span(),
                    "use `lock walk cursor as value { ... }`",
                )),
            );
        }
        // A target that already reported its own error (an unknown name, a bad
        // call) has a placeholder type; a second "wrong lock type" diagnostic on
        // top of it would only be noise.
        let errors_before = self.error_count();
        let target_ty = self.check_expr(&s.target);
        let target_is_broken = self.error_count() > errors_before;
        let protected = if target_is_broken {
            Some(Self::error_type())
        } else {
            self.lock_target_type(s.mode, &s.target, &target_ty)
        };

        // The lock statement parks the task on contention, so it needs an async
        // frame to resume into. The blocking primitive is a separate type
        // (`BlockingCell<T>`); `Mutex<T>` never changes meaning by context.
        if !self.local.current_async_context {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2603,
                    "scheduler-aware lock acquisition is only allowed in an async function",
                )
                .with_label(Label::primary(
                    s.header_span(),
                    format!("`{}` used in a synchronous function", s.mode.keyword()),
                ))
                .with_help(
                    "mark the enclosing function `async fn`, or move the critical section into one",
                ),
            );
        }

        // Evaluate every target in the outer scope, before introducing any
        // protected-value bindings or acquiring any member of the group.
        let mut additional_types = Vec::with_capacity(s.additional.len());
        for member in &s.additional {
            let errors = self.error_count();
            let ty = self.check_expr(&member.target);
            let value_ty = if self.error_count() > errors {
                Self::error_type()
            } else {
                self.lock_target_type(member.mode, &member.target, &ty)
                    .unwrap_or(Type::Never)
            };
            if self.local.current_async_context {
                self.record_async_local(
                    value_ty.clone(),
                    &member.binding,
                    member.binding_span,
                    super::async_retention::Key::binding(&member.binding),
                );
                self.record_async_local(
                    ty,
                    "lock handle",
                    member.target_span,
                    super::async_retention::Key::Value(member.target.id()),
                );
            }
            additional_types.push(value_ty);
        }

        let binding_ty = protected.unwrap_or(Type::Never);
        // An async fn resumes into the critical section after a contended
        // acquisition, so the binding lives in the async frame,
        // and so do the three pieces of compiler-owned state the resumed poll
        // and the cancel entry need: the evaluated lock handle, this
        // acquisition's registration token, and its phase.
        if self.local.current_async_context {
            self.record_async_local(
                binding_ty.clone(),
                &s.binding,
                s.span,
                super::async_retention::Key::binding(&s.binding),
            );
            self.record_async_local(
                target_ty.clone(),
                "lock handle",
                s.target.span(),
                super::async_retention::Key::Value(s.target.id()),
            );
            self.local.async_local_types.push(Type::I64);
            self.local.async_local_types.push(Type::I64);
        }

        self.symbols.push_scope();
        self.define_var(
            s.binding.clone(),
            VarInfo {
                ty: binding_ty,
                mutable: s.mutable,
                is_param: false,
                declaration_span: s.binding_span,
            },
        );
        let mut bindings = std::collections::HashSet::from([s.binding.as_str()]);
        for (member, ty) in s.additional.iter().zip(additional_types) {
            if !bindings.insert(&member.binding) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2601,
                        "duplicate binding in simultaneous lock acquisition",
                    )
                    .with_label(Label::primary(
                        member.binding_span,
                        "binding already declared in this group",
                    )),
                );
            }
            self.define_var(
                member.binding.clone(),
                VarInfo {
                    ty,
                    mutable: member.mutable,
                    is_param: false,
                    declaration_span: member.binding_span,
                },
            );
        }
        self.local.lock_depth += 1;
        self.local.lexical_block_depth += 1;
        for stmt in &s.body.stmts {
            self.check_stmt(stmt);
        }
        self.local.lexical_block_depth -= 1;
        self.local.lock_depth -= 1;
        self.symbols.pop_scope();

        // Direct waits were recorded while checking the body. They are
        // diagnosed with transitive effects after all callable bodies exist.
    }

    /// The protected value type `T` when `target_ty` is the lock type `s.mode`
    /// requires, reporting E2602 and returning `None` when it is not.
    pub(super) fn lock_target_type(
        &mut self,
        mode: LockMode,
        target: &Expr,
        target_ty: &Type,
    ) -> Option<Type> {
        let required = mode.lock_type_name();
        if let Type::Generic(name, args) = target_ty
            && match B::from_name(required) {
                Some(id) => B::from_name(name) == Some(id),
                // Group is a syntax sentinel, not a nominal builtin family.
                None => name == required,
            }
            && args.len() == 1
        {
            return Some(args[0].clone());
        }
        // A checker error upstream already reported itself; do not pile on.
        if Self::contains_error_type(target_ty) {
            return Some(Self::error_type());
        }
        if *target_ty == Type::Never {
            return None;
        }

        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E2602,
            format!(
                "`{}` requires `{required}<T>`, found `{}`",
                mode.keyword(),
                self.diagnostic_type_name(target_ty)
            ),
        )
        .with_label(Label::primary(
            target.span(),
            format!("expected `{required}<T>`"),
        ));
        // The most likely mistake is reaching for the wrong statement form.
        if let Type::Generic(name, args) = target_ty
            && args.len() == 1
        {
            if B::from_name(name) == Some(B::Mutex) {
                diagnostic = diagnostic.with_help("`Mutex<T>` uses `lock <mutex> as [mut] value`");
            } else if B::from_name(name) == Some(B::RwLock) {
                diagnostic = diagnostic.with_help(
                    "`RwLock<T>` uses `lock read <rwlock> as value` or `lock write <rwlock> as [mut] value`",
                );
            }
        }
        self.push(diagnostic);
        None
    }

    pub(in crate::semantic::type_checker) fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => self.check_let_stmt(s),
            Stmt::FieldAssign(s) => self.check_field_assign(s),
            Stmt::StaticFieldAssign(s) => self.check_static_field_assign(s),
            Stmt::SuperInit(s) => self.check_super_init(s),
            Stmt::IndexAssign(s) => self.check_index_assign(s),
            Stmt::Assign(s) => self.check_assign_stmt(s),
            Stmt::If(s) => self.check_if_stmt(s),
            Stmt::While(s) => self.check_while_stmt(s),
            Stmt::For(s) => self.check_for_stmt(s),
            Stmt::Break(span) => self.check_loop_exit("break", *span),
            Stmt::Continue(span) => self.check_loop_exit("continue", *span),
            Stmt::Lock(s) => self.check_lock_stmt(s),
            Stmt::Defer(d) => self.check_defer_stmt(d),
            Stmt::Return(s) => self.check_return_stmt(s),
            Stmt::Expr(s) => self.check_expr_stmt(s),
        }
    }

    pub(super) fn check_loop_exit(&mut self, kw: &str, span: Span) {
        if self.local.loop_depth == 0 {
            self.push_error(
                ErrorCode::E0904,
                format!("`{kw}` outside of a loop"),
                span,
                format!("`{kw}` is only allowed inside `while` or `for`"),
            );
        }
    }

    pub(super) fn check_let_stmt(&mut self, s: &LetStmt) {
        // The annotation is judged before the initializer so a
        // malformed one (`let x: Option = ...`) is reported ahead of
        // the mismatches it goes on to cause.
        let annotation = s.ty.as_ref().map(|ty| {
            let errors_before = self.error_generation;
            let type_span = s.type_span.unwrap_or(s.span);
            let annotation = self.normalize_type(ty, type_span);
            self.validate_type(&annotation, type_span);
            if self.error_generation == errors_before {
                annotation
            } else {
                // Retain the binding without letting its rejected type
                // cause errors in the initializer or subsequent uses.
                Self::error_type()
            }
        });
        let reference_start = self.analysis_symbols.references.len();
        let inferred = match &annotation {
            Some(ann) => self.check_expr_expecting(&s.init, ann),
            None => self.check_expr(&s.init),
        };
        self.copy_capture_cause(s.init.span(), s.span);
        if s.synthetic_kind == crate::qualified_name::SyntheticKind::CompoundArray {
            self.record_element_write(&s.init, reference_start, true);
        }
        let ty = if let Some(ann) = &annotation {
            if !s.mutable && inferred.callable_kinds_refine(ann) {
                // A written `fn(...)` names both callable kinds. An immutable
                // binding keeps what the initializer knows (a shareable
                // function value stays shareable), so the annotation does
                // not, for instance, stop it from being held across an await.
                inferred
            } else {
                if !self.types_compatible(ann, &inferred) {
                    self.push_let_annotation_mismatch(s, ann, &inferred);
                }
                ann.clone()
            }
        } else {
            // Builtin constructor holes are finalized by check_expr,
            // including recovery from malformed payloads. Keep the
            // existing binding check for user-defined generic enums.
            if !matches!(&inferred, Type::Generic(name, _) if matches!(B::from_name(name), Some(B::Option | B::Result)))
                && let Some(diag) = self.unresolved_generic_enum_diagnostic(
                    &s.init,
                    &inferred,
                    s.init.span(),
                    &s.name,
                )
            {
                self.push(diag);
            }
            if matches!(&inferred, Type::Array(element) if **element == Type::Void) {
                self.push(
                    Self::error_at(
                        ErrorCode::E1801,
                        "cannot infer array element type; annotate the binding",
                        s.span,
                        "element type annotation required",
                    )
                    .with_help(format!(
                        "add an element type, e.g. `let {}: Array<i64> = [];`",
                        s.name
                    )),
                );
                // Poison the binding so push, iteration and await do not
                // turn an unresolved element into secondary diagnostics.
                Self::error_type()
            } else {
                inferred
            }
        };
        if annotation.is_none()
            && builtin_types::binary_args(&ty, B::Map)
                .is_some_and(|(key, value)| *key == Type::Void && *value == Type::Void)
        {
            self.local.inferred_maps.insert(
                s.span,
                (
                    vec![s.init.id()],
                    self.local
                        .current_async_context
                        .then_some(self.local.async_local_types.len()),
                ),
            );
        }
        // Record the resolved type of locals inside async fns so the
        // backend can frame-back unannotated live-across-await locals.
        if self.local.current_async_context {
            self.local.async_local_bindings.insert(
                self.local.async_local_types.len(),
                (
                    s.name.clone(),
                    s.span,
                    super::async_retention::Key::binding(&s.name),
                ),
            );
            self.local.async_local_types.push(ty.clone());
        }
        // `_` is a wildcard: evaluate the initializer for side effects but do
        // not bind a variable (allows multiple `let _ = expr;` in the same scope).
        if s.name == "_" {
            return;
        }
        // E0351: reject redeclaration in the same scope.
        if let Some(_prev) = self.symbols.lookup_var_current_scope(&s.name) {
            self.push_error(
                ErrorCode::E0351,
                format!("variable `{}` is already defined in this scope", s.name),
                s.span,
                "previous definition here",
            );
        }
        self.bind_capture_cause(s.init.span(), s.span, &ty);
        self.define_var(
            s.name.clone(),
            VarInfo {
                ty,
                mutable: s.mutable,
                is_param: false,
                declaration_span: s.span,
            },
        );
    }

    /// A `let` initializer incompatible with its annotation. Class-to-class
    /// mismatches (E0704) name the variable and point at the annotation.
    pub(super) fn push_let_annotation_mismatch(
        &mut self,
        s: &LetStmt,
        ann: &Type,
        inferred: &Type,
    ) {
        let code = self.type_mismatch_error_code(ann, inferred);
        if code != ErrorCode::E0704 {
            let diagnostic = self.type_mismatch_diagnostic(code, ann, inferred, s.span);
            self.push(self.with_task_await_help(diagnostic, ann, inferred, &s.init));
            return;
        }
        let expected = self.diagnostic_type_name(ann);
        self.push_error(
            code,
            format!(
                "cannot assign `{}` to variable `{}` of type `{expected}`",
                self.diagnostic_type_name(inferred),
                s.name,
            ),
            s.span,
            format!("expected `{expected}` because of this type annotation"),
        );
    }

    pub(super) fn check_field_assign(&mut self, s: &FieldAssignStmt) {
        let obj_ty = self.check_expr(&s.object);
        let field_ty = self.resolve_field(&obj_ty, &s.field, s.target_span, true, "write");
        let val_ty = if field_ty == Type::Void {
            self.check_expr(&s.value)
        } else {
            self.check_expr_expecting(&s.value, &field_ty)
        };
        if field_ty != Type::Void && !self.types_compatible(&field_ty, &val_ty) {
            self.push_value_type_mismatch(&field_ty, &val_ty, s.span, &s.value);
        }
        if obj_ty == Type::Named("PanicInfo".to_string()) {
            self.push_error(
                ErrorCode::E0201,
                "fields of `PanicInfo` are read-only",
                s.span,
                "runtime panic metadata cannot be modified",
            );
        }
    }

    pub(super) fn check_index_assign(&mut self, s: &IndexAssignStmt) {
        let reference_start = self.analysis_symbols.references.len();
        let arr_ty = self.check_expr(&s.array);
        self.record_element_write(&s.array, reference_start, false);
        let idx_ty = self.check_expr(&s.index);
        if !matches!(idx_ty, Type::I64) && !Self::contains_error_type(&idx_ty) {
            self.push_error(
                ErrorCode::E0201,
                format!(
                    "array index must be `i64`, found `{}`",
                    self.diagnostic_type_name(&idx_ty)
                ),
                s.index.span(),
                "index is not an `i64`",
            );
        }
        match &arr_ty {
            Type::Array(elem) | Type::FixedArray(elem, _) => {
                let val_ty = self.check_expr_expecting(&s.value, elem);
                if !self.types_compatible(elem, &val_ty) {
                    let element = self.diagnostic_type_name(elem);
                    self.push(self.with_callable_kind_help(
                        Self::error_at(
                            self.type_mismatch_error_code(elem, &val_ty),
                            format!(
                                "cannot assign `{}` to an element of `Array<{element}>`",
                                self.diagnostic_type_name(&val_ty),
                            ),
                            s.span,
                            format!("expected `{element}`"),
                        ),
                        elem,
                        &val_ty,
                    ));
                }
            }
            // A known array still provides element context even if a
            // component is erroneous. Other recovery receivers cannot
            // support a not-an-array diagnostic; always check the value.
            ty if *ty == Type::Void || Self::contains_error_type(ty) => {
                self.check_expr(&s.value);
            }
            other => {
                self.check_expr(&s.value);
                self.push_error(
                    ErrorCode::E0201,
                    format!(
                        "cannot index a value of type `{}`",
                        self.diagnostic_type_name(other)
                    ),
                    s.span,
                    "not an array",
                );
            }
        }
    }

    pub(super) fn check_assign_stmt(&mut self, s: &AssignStmt) {
        self.record_binding_use(&s.name, s.span, "write");
        if s.name == "this" {
            self.push_legacy_this_error(s.span);
            return;
        }
        // Reject direct assignment to `self`.
        if s.name == "self" {
            self.push(
                Self::error_at(
                    ErrorCode::E0552,
                    format!("cannot assign to `{}`", s.name),
                    s.span,
                    "cannot assign to receiver",
                )
                .with_help(format!("to mutate fields, use `{}.field = value`", s.name)),
            );
            return;
        }
        let info = self.symbols.lookup_var(&s.name).cloned();
        let constant = info
            .is_none()
            .then(|| self.symbols.lookup_func(&s.name))
            .flatten()
            .filter(|f| f.constant.is_some())
            .map(|f| f.declaration_span);
        match info {
            None if constant.is_some() => self.push(
                Self::error_at(
                    ErrorCode::E0301,
                    format!("cannot assign to constant `{}`", s.name),
                    s.span,
                    "cannot assign",
                )
                .with_label(Label::secondary(
                    constant.unwrap_or(s.span),
                    "declared as a constant here",
                ))
                .with_help(format!(
                    "declare a mutable local instead: `let mut {} = ...`",
                    s.name.to_lowercase()
                )),
            ),
            None => self.push_error(
                ErrorCode::E0350,
                format!("cannot find variable `{}`", s.name),
                s.span,
                "not found in this scope",
            ),
            Some(info) => {
                // A write to a capture was already reported against the
                // capture; reporting the enclosing
                // local's mutability on top of it would point at a fix
                // that does not exist.
                if !info.mutable && !self.local.capture_writes.contains(&s.span) {
                    self.push_immutable_assignment(s, &info);
                }
                let got = self.check_expr_expecting(&s.value, &info.ty);
                self.copy_capture_cause(s.value.span(), s.span);
                self.bind_capture_cause(s.value.span(), info.declaration_span, &info.ty);
                if !self.types_compatible(&info.ty, &got) {
                    self.push_value_type_mismatch(&info.ty, &got, s.span, &s.value);
                }
            }
        }
    }

    /// A write to an immutable local or parameter.
    pub(super) fn push_immutable_assignment(&mut self, s: &AssignStmt, info: &VarInfo) {
        if info.is_param {
            self.push(
                Self::error_at(
                    ErrorCode::E0302,
                    format!("cannot assign to immutable parameter `{}`", s.name),
                    s.span,
                    "cannot assign to parameter",
                )
                .with_help(format!(
                    "introduce a mutable local variable: `let mut {} = {};`",
                    s.name, s.name
                )),
            );
        } else {
            // Build an insertion span just after "let " in the declaration.
            let decl = info.declaration_span;
            let insert_span = Span::in_file(
                decl.file_id,
                decl.start + 4,
                decl.start + 4,
                decl.line,
                decl.col + 4,
            );
            self.push(
                Self::error_at(
                    ErrorCode::E0301,
                    format!("cannot assign to immutable variable `{}`", s.name),
                    s.span,
                    "cannot assign",
                )
                .with_label(Label::secondary(
                    info.declaration_span,
                    "declared immutable here",
                ))
                .with_help(format!("declare it as mutable: `let mut {} = ...`", s.name))
                .with_fix(FixSuggestion::insertion(
                    insert_span,
                    "mut ",
                    "add `mut` here",
                )),
            );
        }
    }

    pub(super) fn check_if_stmt(&mut self, s: &IfStmt) {
        let errors_before = self.error_generation;
        let cond_ty = self.check_expr(&s.cond);
        self.check_condition_is_bool(&s.cond, &cond_ty, errors_before);
        self.check_block(&s.then_block);
        if let Some(else_b) = &s.else_block {
            self.check_block(else_b);
        }
    }

    pub(super) fn check_while_stmt(&mut self, s: &WhileStmt) {
        self.check_loop_contracts(&s.invariants, s.variant.as_ref());
        let errors_before = self.error_generation;
        let cond_ty = self.check_expr(&s.cond);
        self.check_condition_is_bool(&s.cond, &cond_ty, errors_before);
        self.local.loop_depth += 1;
        self.check_block(&s.body);
        self.local.loop_depth -= 1;
    }

    pub(super) fn check_for_stmt(&mut self, s: &ForStmt) {
        let iterable_ty = self.check_expr(&s.iterable);
        let elem_ty = match &iterable_ty {
            ty if Self::contains_error_type(ty) => Self::error_type(),
            Type::Array(elem) | Type::FixedArray(elem, _) => (**elem).clone(),
            ty if builtin_types::binary_args(ty, B::Map).is_some() => {
                let (key, value) = builtin_types::binary_args(ty, B::Map).unwrap();
                Type::Generic("$Tuple2".into(), vec![key.clone(), value.clone()])
            }
            ty if builtin_types::unary_arg(ty, B::FrozenArray).is_some() => {
                builtin_types::unary_arg(ty, B::FrozenArray)
                    .unwrap()
                    .clone()
            }
            Type::Generic(name, args)
                if B::from_name(name) == Some(B::Range) && args.as_slice() == [Type::I64] =>
            {
                Type::I64
            }
            Type::Void => Type::Void,
            other => {
                let iterable_name = match &s.iterable {
                    Expr::Var(name, ..) => Some(name.as_str()),
                    _ => None,
                };
                let help = match builtin_types::resolve(other).map(|ty| ty.id) {
                    Some(B::FrozenMap) => {
                        if let Some(name) = iterable_name {
                            format!("`FrozenMap` has no iterator; keep keys in an array and retrieve entries with `{name}.get(key)`")
                        } else {
                            "`FrozenMap` has no iterator; keep keys in an array and retrieve entries with `.get(key)`".into()
                        }
                    }
                    _ => {
                        "use `for item in array { ... }` with `Array<T>` / `FrozenArray<T>` or `for n in start..end { ... }`".into()
                    }
                };
                self.push(
                    Self::error_at(
                        ErrorCode::E0201,
                        format!("cannot iterate over `{}`", self.diagnostic_type_name(other)),
                        s.iterable.span(),
                        "for-in requires an array, Map, or i64 range",
                    )
                    .with_help(help),
                );
                Self::error_type()
            }
        };

        if self.local.current_async_context {
            let iter_slot_ty = if is_i64_range_type(&iterable_ty) {
                Type::I64
            } else {
                iterable_ty.clone()
            };
            self.record_async_local(
                iter_slot_ty,
                "for iterator",
                s.iterable.span(),
                super::async_retention::Key::Value(s.iterable.id()),
            );
            self.local.async_local_types.push(Type::I64);
            if s.name != "_" {
                self.record_async_local(
                    elem_ty.clone(),
                    &s.name,
                    s.name_span,
                    super::async_retention::Key::binding(&s.name),
                );
            }
        }

        self.symbols.push_scope();
        if s.name != "_" {
            self.define_var(
                s.name.clone(),
                VarInfo {
                    ty: elem_ty,
                    mutable: false,
                    is_param: false,
                    declaration_span: s.name_span,
                },
            );
        }
        self.symbols.push_scope();
        if !matches!(s.iterable, Expr::Range(_)) {
            self.define_var(
                "iteration".into(),
                VarInfo {
                    ty: Type::I64,
                    mutable: false,
                    is_param: false,
                    declaration_span: s.span,
                },
            );
        }
        self.check_loop_contracts(&s.invariants, s.variant.as_ref());
        self.symbols.pop_scope();
        self.local.loop_depth += 1;
        self.local.lexical_block_depth += 1;
        for stmt in &s.body.stmts {
            self.check_stmt(stmt);
        }
        self.local.lexical_block_depth -= 1;
        self.local.loop_depth -= 1;
        self.symbols.pop_scope();
        self.end_capture_scope(&s.body);
    }

    pub(super) fn check_defer_stmt(&mut self, d: &DeferStmt) {
        self.check_defer_body_shape(d);
        let capture_entry = matches!(
            d.body,
            DeferBody::Block(_) | DeferBody::Expr(Expr::Match(_))
        )
        .then(|| self.local.capture_bindings.clone());
        match &d.body {
            // The deferred `await` itself is the reported mistake; checking it
            // would add an unrelated "await outside async" error.
            DeferBody::Expr(Expr::Await(awaited)) => {
                self.check_expr(&awaited.expr);
            }
            DeferBody::Expr(expr) => {
                self.check_expr(expr);
            }
            DeferBody::Block(block) => self.check_block(block),
        }
        if let Some(entry) = capture_entry {
            self.local.capture_bindings = entry;
        }
        if let Some((span, operation)) = defer_scheduler_drive_span(&d.body, &self.builtin_calls) {
            self.push(
                Self::error_at(
                    ErrorCode::E0905,
                    "scheduler-driving operations are not allowed inside a `defer`",
                    span,
                    format!("`{operation}` could run or suspend another task during cleanup"),
                )
                .with_help("perform the operation explicitly before leaving the scope"),
            );
        }
        if self.local.current_async_context {
            self.record_defer_async_operands(d);
        }
        // An ASYNC callee would only SPAWN a task at scope exit — the
        // cleanup body would never be driven to completion. Reject
        // until async defer (Phase 3) defines this (review fix).
        if let DeferBody::Expr(Expr::Await(awaited)) = &d.body {
            // One mistake, one error: the shape and suspension checks skip a
            // directly deferred `await`, which is reported here instead.
            let message = if matches!(
                awaited.expr,
                Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
            ) {
                "cannot `defer` an async call"
            } else {
                "cannot `defer` an `await`"
            };
            self.push(
                Self::error_at(
                    ErrorCode::E0905,
                    message,
                    awaited.span,
                    "deferred cleanup runs synchronously and cannot wait for a task",
                )
                .with_help(DEFER_ASYNC_HELP),
            );
        } else if let Some(span) = defer_async_call_span(&d.body, &self.expr_types) {
            self.push(
                Self::error_at(
                    ErrorCode::E0905,
                    "cannot `defer` an async call",
                    span,
                    "this would only spawn a task at scope exit, not run it to completion",
                )
                .with_help(DEFER_ASYNC_HELP),
            );
        }
    }

    /// Syntactic `defer` restrictions that need no types: the accepted
    /// body forms, recovery placement, control flow, suspension and
    /// reference arguments.
    pub(super) fn check_defer_body_shape(&mut self, d: &DeferStmt) {
        // A direct call keeps the original defer contract: receiver
        // and arguments are evaluated at registration. A match/block
        // is evaluated at scope exit, allowing explicit Result
        // handling without changing the parent's result/control flow.
        // A directly deferred `await` is reported once, after typing, and a
        // directly deferred `select` by the suspension check below.
        let direct_await = matches!(&d.body, DeferBody::Expr(Expr::Await(_)));
        let supported = direct_await
            || matches!(&d.body, DeferBody::Expr(Expr::Select(_)))
            || matches!(
                &d.body,
                DeferBody::Expr(
                    Expr::Call(_)
                        | Expr::StaticCall(_)
                        | Expr::MethodCall(_)
                        | Expr::Print(..)
                        | Expr::Match(_)
                ) | DeferBody::Block(_)
            );
        if !supported {
            self.push_error(
                ErrorCode::E0905,
                "`defer` expects a call, match expression, or block",
                d.body.span(),
                "use `defer f(args);`, `defer match value { ... }`, or `defer { ... }`",
            );
        }
        if matches!(
            &d.body,
            DeferBody::Expr(Expr::Call(call)) if call.callee == "recover"
        ) {
            self.push(
                Self::error_at(
                    ErrorCode::E0905,
                    "`defer recover();` discards the panic metadata",
                    d.body.span(),
                    "handle `recover()` explicitly in a deferred block or match",
                )
                .with_help("use `defer match recover() { Some(info) => { ... }, None => {} }`"),
            );
        }
        let recovery_capable = defer_body_contains_direct_recover(&d.body);
        if recovery_capable
            && self.local.lexical_block_depth == 1
            && self.local.current_return_type != Type::Void
        {
            self.push(Self::error_at(ErrorCode::E0905, "an outermost recovery scope cannot complete a non-void function", d.body.span(), "recovery here would leave the function without a return value").with_help(
                    "put the recovery-capable defer in a nested block and return a value after that block",
                ));
        }

        for (span, keyword) in defer_control_flow_violations(&d.body) {
            self.local.capture_flow.invalid_defers.insert(d.body.span());
            self.push_error(
                ErrorCode::E0905,
                format!("`{keyword}` is not allowed inside a `defer`"),
                span,
                "a deferred body cannot change its enclosing function or loop",
            );
        }

        if !direct_await && defer_body_contains_suspend(&d.body) {
            // The registration happens at the defer statement; an
            // async operation inside the deferred body has no suspension point
            // to resume from at flush/cancel time.
            self.push(
                Self::error_at(
                    ErrorCode::E0905,
                    "`await` and `select` are not allowed inside a `defer`",
                    d.body.span(),
                    "deferred bodies run synchronously",
                )
                .with_help(DEFER_ASYNC_HELP),
            );
        }
        // Reference arguments would act on a hidden COPY of the value
        // (operands are stashed at registration), silently missing the
        // caller's variable — reject them in v1 (review fix).
        let ref_arg_span = match &d.body {
            DeferBody::Expr(Expr::Call(c)) => c
                .args
                .iter()
                .find(|a| !matches!(a.mode, CallArgMode::Value))
                .map(|a| a.span),
            DeferBody::Expr(Expr::StaticCall(c)) => c
                .args
                .iter()
                .find(|a| !matches!(a.mode, CallArgMode::Value))
                .map(|a| a.span),
            DeferBody::Expr(Expr::MethodCall(m)) => m
                .args
                .iter()
                .find(|a| !matches!(a.mode, CallArgMode::Value))
                .map(|a| a.span),
            _ => None,
        };
        if let Some(span) = ref_arg_span {
            self.push(
                Self::error_at(
                    ErrorCode::E0905,
                    "reference arguments are not supported in `defer`",
                    span,
                    "the deferred call would mutate a hidden copy, not this variable",
                )
                .with_help("wrap the mutation in a class method and defer that instead"),
            );
        }
    }

    /// Async defer: operands are stashed into the task frame at
    /// registration, so record their types (keyed by operand) for codegen
    /// to lay out and GC-mask the slots, plus a deferred local callee.
    pub(super) fn record_defer_async_operands(&mut self, d: &DeferStmt) {
        let mut record = |expr: &Expr| {
            let ty = self
                .expr_types
                .get(&expr.id())
                .cloned()
                .unwrap_or(Type::I64);
            self.record_async_local(
                ty,
                "deferred operand",
                expr.span(),
                super::async_retention::Key::Value(expr.id()),
            );
        };
        match &d.body {
            DeferBody::Expr(Expr::Call(c)) => c.args.iter().for_each(|a| record(&a.expr)),
            DeferBody::Expr(Expr::StaticCall(c)) => c.args.iter().for_each(|a| record(&a.expr)),
            DeferBody::Expr(Expr::MethodCall(m)) => {
                record(&m.object);
                m.args.iter().for_each(|a| record(&a.expr));
            }
            DeferBody::Expr(Expr::Print(arg, ..)) => record(arg),
            _ => {}
        }
        if let DeferBody::Expr(Expr::Call(c)) = &d.body
            && let Some(info) = self.symbols.lookup_var(&c.callee)
        {
            self.record_async_local(
                info.ty.clone(),
                "deferred callee",
                c.span,
                super::async_retention::Key::DeferredCallee(c.id),
            );
        }
    }

    pub(super) fn check_return_stmt(&mut self, s: &ReturnStmt) {
        // In a constructor, a bare `return;` is fine but `return <value>`
        // is rejected (E0841).
        if self.local.in_constructor {
            if let Some(v) = &s.value {
                self.check_expr(v);
                self.push(
                    Self::error_at(
                        ErrorCode::E0841,
                        "constructor `init` cannot return a value",
                        s.span,
                        "remove the returned value",
                    )
                    .with_help("a constructor implicitly returns the new object"),
                );
            }
            return;
        }
        // `return Result::Ok();` (zero-arg) is the success value of a
        // `Result<void, E>` function: the Ok payload is void, so no
        // argument is required.
        if let Some(Expr::StaticCall(sc)) = &s.value {
            let returns_result_void =
                builtin_types::binary_args(&self.local.current_return_type, B::Result)
                    .is_some_and(|(ok, _)| *ok == Type::Void);
            if returns_result_void
                && B::from_name(&sc.class) == Some(B::Result)
                && sc.method == "Ok"
                && sc.args.is_empty()
            {
                // Record the type anyway: nothing else visits this call,
                // and HIR lowering asks the checker for the type of any
                // static call it cannot resolve itself.
                self.expr_types
                    .insert(sc.id, self.local.current_return_type.clone());
                self.static_call_classes.insert(
                    sc.id,
                    crate::semantic::ids::TypeId::from_source_name(&sc.class),
                );
                return;
            }
        }
        let ret_ty = match &s.value {
            // Resolve an unqualified variant in `return Ok(42)` against
            // the function's return type. Skipped inside a
            // lambda, where `current_return_type` is not the lambda's.
            Some(v) if self.local.lambda_return_stack.is_empty() => {
                let expected = self.local.current_return_type.clone();
                self.check_expr_expecting(v, &expected)
            }
            Some(v) => self.check_lambda_return_value(v),
            None => Type::Void,
        };
        // Inside a lambda with no annotation: record the return type for inference.
        if let Some(slot) = self.local.lambda_return_stack.last_mut() {
            if slot.is_none() {
                *slot = Some(ret_ty.clone());
            }
            return; // don't validate against outer current_return_type
        }
        if !self.types_compatible(&self.local.current_return_type, &ret_ty) {
            let expected = self.local.current_return_type.clone();
            match &s.value {
                Some(value) => self.push_value_type_mismatch(&expected, &ret_ty, s.span, value),
                None => self.push_type_mismatch(&expected, &ret_ty, s.span),
            }
        }
    }

    pub(super) fn check_expr_stmt(&mut self, s: &ExprStmt) {
        let errors_before = self.error_generation;
        let ty = self.check_expr(&s.expr);
        // Inspect the already computed outer type, without traversing or
        // cloning its result type. Value-consuming contexts never enter
        // this statement branch.
        // Generic prechecking revisits these bodies in the final pass;
        // emit this advisory only with the final resolved types.
        // `scope.add(task)` hands the task to the scope, whose
        // `finish()` joins it; its returned handle is optional.
        let scope_owned = matches!(&s.expr, Expr::MethodCall(call)
            if self.builtin_calls.get(&call.id) == Some(&Intrinsic::ScopeAdd));
        if !self.template_precheck
            && self.error_generation == errors_before
            && !scope_owned
            && (is_task_handle_type(&ty) || builtin_types::is(&ty, B::TaskResult))
        {
            self.push(
                Diagnostic::new(
                    Severity::Warning,
                    ErrorCode::W0802,
                    "task or future value discarded without awaiting",
                )
                .with_label(Label::primary(s.expr.span(), "unused awaitable value"))
                .with_help("use `await ...` to wait for completion, or explicitly bind the value with `let task = ...;` (or `let _ = ...;`) to discard it intentionally"),
            );
        }
    }
}
