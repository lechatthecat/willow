//! The `check_*` type-checking methods (extracted from `mod.rs`). `check_program`
//! stays `pub` (the entry point); the rest are `pub(super)`. As a child module
//! these reach `TypeChecker`'s private fields/methods.

use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
#[cfg(test)]
use crate::lexer::Lexer;
#[cfg(test)]
use crate::parser::Parser;
use crate::parser::ast::*;
use crate::semantic::symbols::*;

use super::*;

impl TypeChecker {
    /// Report E2003 if `name` (a local declaration) collides with an imported
    /// name (a module access name or a directly imported item).
    pub(super) fn check_local_decl_collision(&mut self, name: &str, span: Span) {
        if let Some(import_span) = self.imported_names.get(name).copied() {
            let mut diag = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2003,
                format!("name `{name}` is defined both by an import and a local declaration"),
            )
            .with_label(Label::primary(span, "local declaration here"));
            if let Some(s) = import_span {
                diag = diag.with_label(Label::secondary(s, "imported here"));
            }
            self.push(diag.with_help("rename the local declaration or the import"));
        }
    }

    pub fn check_program(&mut self, program: &Program) {
        self.register_std_imports(&program.imports);
        // Looping sync helpers indexed by `Class::method`, so a call through a
        // typed non-`self` receiver (`obj.heavy()`) in a task context is flagged
        // E0810 — the AST-only ConcurrencyAnalyzer cannot resolve the receiver
        // type (willow-0a6k.2).
        self.nonpreemptible_methods =
            crate::semantic::concurrency::compute_nonpreemptible_helpers(program);

        // Pass 1: register class shapes, enum declarations, and interfaces.
        // Interfaces share the top-level namespace with classes/enums/functions
        // and must be registered before class conformance is validated.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.check_local_decl_collision(&c.name, c.span);
                    self.register_class(c);
                }
                Item::Enum(e) => {
                    self.check_local_decl_collision(&e.name, e.span);
                    self.register_enum(e);
                }
                Item::Interface(i) => {
                    self.check_local_decl_collision(&i.name, i.span);
                    self.register_interface(i, None);
                }
                _ => {}
            }
        }

        // Pass 2: register all top-level function signatures
        for item in &program.items {
            if let Item::Function(f) = item {
                self.check_local_decl_collision(&f.name, f.span);
                let params = self.normalize_param_types(&f.params);
                let param_infos = self.normalize_param_infos(&f.params);
                let return_type = self.normalize_type(&f.return_type, f.span);
                self.symbols.define_func(
                    f.name.clone(),
                    FuncInfo {
                        param_infos,
                        params,
                        return_type,
                        public: f.public,
                        is_async: f.is_async,
                        declaration_span: f.span,
                        module_path: None,
                    },
                );
            }
        }

        // Pass 3: check bodies
        for item in &program.items {
            match item {
                Item::Function(f) => self.check_function(f),
                Item::Class(c) => self.check_class(c),
                Item::Enum(_) => {} // already registered
                Item::Interface(i) => self.check_interface(i), // validate `extends`
            }
        }
    }

    pub(super) fn check_block(&mut self, block: &Block) {
        self.symbols.push_scope();
        self.narrowed_vars.push(HashMap::new());
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.narrowed_vars.pop();
        self.symbols.pop_scope();
    }

    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let annotation = s.ty.as_ref().map(|ty| self.normalize_type(ty, s.span));
                // A `let xs: Array<I> = [..]` literal is checked element-wise
                // against `I`, so classes implementing interface `I` are accepted.
                let inferred = match (&annotation, &s.init) {
                    (Some(Type::Array(elem)), Expr::ArrayLiteral(elements, lit_span)) => {
                        self.check_array_literal_expecting(elements, *lit_span, Some(elem.as_ref()))
                    }
                    (Some(ann), _) => self.check_expr_expecting(&s.init, ann),
                    _ => self.check_expr(&s.init),
                };
                let ty = if let Some(ann) = &annotation {
                    self.validate_type(ann, s.span);
                    let channel_new_infers_from_annotation =
                        channel_element_type(ann).is_some() && is_untyped_channel_new_call(&s.init);
                    if !channel_new_infers_from_annotation && !self.types_compatible(ann, &inferred)
                    {
                        let code = self.type_mismatch_error_code(ann, &inferred);
                        let message = if code == ErrorCode::E0704 {
                            format!(
                                "cannot assign `{}` to variable `{}` of type `{}`",
                                type_name(&inferred),
                                s.name,
                                type_name(ann)
                            )
                        } else {
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&inferred)
                            )
                        };
                        let label = if code == ErrorCode::E0704 {
                            format!(
                                "expected `{}` because of this type annotation",
                                type_name(ann)
                            )
                        } else {
                            format!("expected `{}`", type_name(ann))
                        };
                        self.push(
                            Diagnostic::new(Severity::Error, code, message)
                                .with_label(Label::primary(s.span, label)),
                        );
                    }
                    ann.clone()
                } else {
                    if inferred == Type::Nil {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                "cannot infer the type of `nil`",
                            )
                            .with_label(Label::primary(
                                s.init.span(),
                                "`nil` needs a nullable type",
                            ))
                            .with_help(
                                "add a nullable type annotation, e.g. `let value: Node? = nil;`",
                            ),
                        );
                    } else if let Some(diag) = self.unresolved_generic_enum_diagnostic(
                        &s.init,
                        &inferred,
                        s.init.span(),
                        &s.name,
                    ) {
                        self.push(diag);
                    }
                    inferred
                };
                // Record the resolved type of locals inside async fns so the
                // backend can frame-back unannotated live-across-await locals
                // (willow-lpn.5c).
                if self.current_async_context {
                    self.async_local_types.insert(s.span, ty.clone());
                }
                // `_` is a wildcard: evaluate the initializer for side effects but do
                // not bind a variable (allows multiple `let _ = expr;` in the same scope).
                if s.name == "_" {
                    return;
                }
                // E0351: reject redeclaration in the same scope.
                if let Some(_prev) = self.symbols.lookup_var_current_scope(&s.name) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0351,
                            format!("variable `{}` is already defined in this scope", s.name),
                        )
                        .with_label(Label::primary(s.span, "previous definition here")),
                    );
                }
                self.symbols.define_var(
                    s.name.clone(),
                    VarInfo {
                        ty,
                        mutable: s.mutable,
                        is_param: false,
                        declaration_span: s.span,
                    },
                );
            }
            Stmt::FieldAssign(s) => {
                let obj_ty = self.check_expr(&s.object);
                let field_ty = self.resolve_field(&obj_ty, &s.field, s.span, true);
                let val_ty = if field_ty == Type::Void {
                    self.check_expr(&s.value)
                } else {
                    self.check_expr_expecting(&s.value, &field_ty)
                };
                if field_ty != Type::Void && !self.types_compatible(&field_ty, &val_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&field_ty, &val_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&field_ty),
                                type_name(&val_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&field_ty)),
                        )),
                    );
                }
            }
            Stmt::StaticFieldAssign(s) => self.check_static_field_assign(s),
            Stmt::SuperInit(s) => self.check_super_init(s),
            Stmt::IndexAssign(s) => {
                let arr_ty = self.check_expr(&s.array);
                let idx_ty = self.check_expr(&s.index);
                if !matches!(idx_ty, Type::I64) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                        )
                        .with_label(Label::primary(s.index.span(), "index is not an `i64`")),
                    );
                }
                match &arr_ty {
                    Type::Array(elem) => {
                        let val_ty = self.check_expr_expecting(&s.value, elem);
                        if !self.types_compatible(elem, &val_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(elem, &val_ty),
                                    format!(
                                        "cannot assign `{}` to an element of `Array<{}>`",
                                        type_name(&val_ty),
                                        type_name(elem)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(elem)),
                                )),
                            );
                        }
                    }
                    Type::Void => {
                        self.check_expr(&s.value);
                    }
                    other => {
                        self.check_expr(&s.value);
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot index a value of type `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(s.span, "not an array")),
                        );
                    }
                }
            }
            Stmt::Assign(s) => {
                if s.name == "this" {
                    self.push_legacy_this_error(s.span);
                    return;
                }
                // Reject direct assignment to `self`.
                if s.name == "self" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0552,
                            format!("cannot assign to `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "cannot assign to receiver"))
                        .with_help(format!("to mutate fields, use `{}.field = value`", s.name)),
                    );
                    return;
                }
                let info = self.symbols.lookup_var(&s.name).cloned();
                match info {
                    None => self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("cannot find variable `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "not found in this scope")),
                    ),
                    Some(info) => {
                        if !info.mutable {
                            if info.is_param {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0302,
                                        format!(
                                            "cannot assign to immutable parameter `{}`",
                                            s.name
                                        ),
                                    )
                                    .with_label(Label::primary(
                                        s.span,
                                        "cannot assign to parameter",
                                    ))
                                    .with_help(format!(
                                        "introduce a mutable local variable: `let mut {} = {};`",
                                        s.name, s.name
                                    )),
                                );
                            } else {
                                // Build an insertion span just after "let " in the declaration.
                                let decl = info.declaration_span;
                                let insert_span = Span::new(
                                    decl.start + 4,
                                    decl.start + 4,
                                    decl.line,
                                    decl.col + 4,
                                );
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0301,
                                        format!("cannot assign to immutable variable `{}`", s.name),
                                    )
                                    .with_label(Label::primary(s.span, "cannot assign"))
                                    .with_label(Label::secondary(
                                        info.declaration_span,
                                        "declared immutable here",
                                    ))
                                    .with_help(format!(
                                        "declare it as mutable: `let mut {} = ...`",
                                        s.name
                                    ))
                                    .with_fix(
                                        FixSuggestion::insertion(
                                            insert_span,
                                            "mut ",
                                            "add `mut` here",
                                        ),
                                    ),
                                );
                            }
                        }
                        let got = self.check_expr_expecting(&s.value, &info.ty);
                        if !self.types_compatible(&info.ty, &got) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(&info.ty, &got),
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(&info.ty),
                                        type_name(&got)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(&info.ty)),
                                )),
                            );
                        }
                        self.clear_narrowing(&s.name);
                    }
                }
            }
            Stmt::If(s) => {
                let cond_ty = self.check_expr(&s.cond);
                let nil_narrowing = self.nil_check_narrowing(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                match nil_narrowing.as_ref() {
                    Some(narrowing) if narrowing.non_nil_when_true => {
                        self.check_block_with_narrowing(&s.then_block, narrowing);
                    }
                    _ => self.check_block(&s.then_block),
                }
                if let Some(else_b) = &s.else_block {
                    match nil_narrowing.as_ref() {
                        Some(narrowing) if !narrowing.non_nil_when_true => {
                            self.check_block_with_narrowing(else_b, narrowing);
                        }
                        _ => self.check_block(else_b),
                    }
                } else if let Some(narrowing) = nil_narrowing.as_ref()
                    && !narrowing.non_nil_when_true
                    && block_always_returns(&s.then_block)
                {
                    self.add_narrowing_to_current_scope(narrowing);
                }
            }
            Stmt::While(s) => {
                let cond_ty = self.check_expr(&s.cond);
                let nil_narrowing = self.nil_check_narrowing(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                match nil_narrowing.as_ref() {
                    Some(narrowing) if narrowing.non_nil_when_true => {
                        self.check_block_with_narrowing(&s.body, narrowing);
                    }
                    _ => self.check_block(&s.body),
                }
            }
            Stmt::For(s) => {
                let iterable_ty = self.check_expr(&s.iterable);
                let elem_ty = match &iterable_ty {
                    Type::Array(elem) => (**elem).clone(),
                    Type::Generic(name, args)
                        if name == "Range" && args.as_slice() == [Type::I64] =>
                    {
                        Type::I64
                    }
                    Type::Void => Type::Void,
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot iterate over `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(
                                s.iterable.span(),
                                "for-in requires an array or i64 range",
                            ))
                            .with_help(
                                "use `for item in array { ... }` with `Array<T>` or `for n in start..end { ... }`",
                            ),
                        );
                        Type::Void
                    }
                };

                if self.current_async_context {
                    let iter_slot_ty = if is_i64_range_type(&iterable_ty) {
                        Type::I64
                    } else {
                        iterable_ty.clone()
                    };
                    self.async_local_types
                        .insert(s.iter_frame_key(), iter_slot_ty);
                    self.async_local_types
                        .insert(s.index_frame_key(), Type::I64);
                    if s.name != "_" {
                        self.async_local_types.insert(s.name_span, elem_ty.clone());
                    }
                }

                self.symbols.push_scope();
                self.narrowed_vars.push(HashMap::new());
                if s.name != "_" {
                    self.symbols.define_var(
                        s.name.clone(),
                        VarInfo {
                            ty: elem_ty,
                            mutable: false,
                            is_param: false,
                            declaration_span: s.name_span,
                        },
                    );
                }
                for stmt in &s.body.stmts {
                    self.check_stmt(stmt);
                }
                self.narrowed_vars.pop();
                self.symbols.pop_scope();
            }
            Stmt::Return(s) => {
                // In a constructor, a bare `return;` is fine but `return <value>`
                // is rejected (willow-scq2 §8 → E0841).
                if self.in_constructor {
                    if let Some(v) = &s.value {
                        self.check_expr(v);
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0841,
                                "constructor `init` cannot return a value",
                            )
                            .with_label(Label::primary(s.span, "remove the returned value"))
                            .with_help("a constructor implicitly returns the new object"),
                        );
                    }
                    return;
                }
                // `return Result::Ok();` (zero-arg) is the success value of a
                // `Result<void, E>` function: the Ok payload is void, so no
                // argument is required (willow-exg).
                if let Some(Expr::StaticCall(sc)) = &s.value {
                    let returns_result_void = matches!(
                        &self.current_return_type,
                        Type::Generic(n, args)
                            if n == "Result" && args.len() == 2 && args[0] == Type::Void
                    );
                    if returns_result_void
                        && sc.class == "Result"
                        && sc.method == "Ok"
                        && sc.args.is_empty()
                    {
                        return;
                    }
                }
                let ret_ty = match &s.value {
                    // Resolve an unqualified variant in `return Ok(42)` against
                    // the function's return type (willow-60o.1). Skipped inside a
                    // lambda, where `current_return_type` is not the lambda's.
                    Some(v) if self.lambda_return_stack.is_empty() => {
                        let expected = self.current_return_type.clone();
                        self.check_expr_expecting(v, &expected)
                    }
                    Some(v) => self.check_expr(v),
                    None => Type::Void,
                };
                // Inside a lambda with no annotation: record the return type for inference.
                if let Some(slot) = self.lambda_return_stack.last_mut() {
                    if slot.is_none() {
                        *slot = Some(ret_ty.clone());
                    }
                    return; // don't validate against outer current_return_type
                }
                if !self.types_compatible(&self.current_return_type, &ret_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&self.current_return_type, &ret_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&self.current_return_type),
                                type_name(&ret_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&self.current_return_type)),
                        )),
                    );
                }
            }
            Stmt::Expr(s) => {
                self.check_expr(&s.expr);
            }
        }
    }

    pub(super) fn check_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.check_expr_inner(expr);
        // Record the authoritative expression type for downstream consumers
        // (HIR lowering, willow-mb5): keyed by span, so the immutable AST
        // never needs to be re-derived.
        self.expr_types.insert(expr.span(), ty.clone());
        ty
    }

    fn check_expr_inner(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Integer(_, _) => Type::I64,
            Expr::Float(_, _) => Type::F64,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Nil(_) => Type::Nil,
            Expr::String(_, _) => Type::String,
            Expr::Var(name, span) => {
                if name == "this" {
                    self.push_legacy_this_error(*span);
                    return Type::Void;
                }
                // Local variable?
                if let Some(info) = self.symbols.lookup_var(name) {
                    if let Some(narrowed_ty) = self.lookup_narrowed_type(name) {
                        return narrowed_ty;
                    }
                    return info.ty.clone();
                }
                // Named function used as a value: `apply(10, double)` where `double: fn(...)`
                if let Some(info) = self.symbols.lookup_func(name) {
                    let params = info.params.clone();
                    let ret = info.return_type.clone();
                    return Type::Fn(params, Box::new(ret));
                }
                // Give a specialized error for receiver keywords used outside instance methods.
                if name == "self" {
                    let diag = if self.in_static_method {
                        let where_ = self
                            .current_class
                            .as_deref()
                            .map(|c| format!(" `{}`", c))
                            .unwrap_or_default();
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0831,
                            format!("`self` is not available in static method{}", where_),
                        )
                        .with_label(Label::primary(*span, "`self` in a static method"))
                        .with_help(
                            "static methods have no receiver; use an instance method instead",
                        )
                    } else if self.in_static_initializer {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0837,
                            "`self` is not available in a static property initializer",
                        )
                        .with_label(Label::primary(*span, "`self` in static initializer"))
                        .with_help("static initializers run before any instance exists")
                    } else {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0550,
                            "`self` can only be used inside an instance method",
                        )
                        .with_label(Label::primary(*span, "`self` used outside instance method"))
                        .with_help(
                            "declare the method without `static` to make it an instance method",
                        )
                    };
                    self.push(diag);
                    return Type::Void;
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find variable `{}`", name),
                    )
                    .with_label(Label::primary(*span, "not found in this scope")),
                );
                Type::I64
            }
            Expr::Binary(b) => self.check_binary(b),
            Expr::Unary(u) => self.check_unary(u),
            Expr::Call(c) => {
                if c.callee == "format" {
                    return self.check_format_call(c);
                }
                // Variadic formatted panic (willow-csax): `panic(spec, args...)`.
                // The one-argument form stays a plain-String builtin call.
                if c.callee == "panic" && c.args.len() != 1 {
                    return self.check_panic_interpolation(c);
                }

                // Direct call to a named function.
                if let Some(info) = self.symbols.lookup_func(&c.callee).cloned() {
                    if info.params.len() != c.args.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "function `{}` takes {} argument(s) but {} were supplied",
                                    c.callee,
                                    info.params.len(),
                                    c.args.len()
                                ),
                            )
                            .with_label(Label::primary(c.span, "wrong number of arguments")),
                        );
                    }
                    self.check_call_args_against_param_infos(&info.param_infos, &c.args);
                    // Calling an async fn captures its arguments into a Task that
                    // may cross a worker boundary — enforce Send/Sync (dgwo.4).
                    if info.is_async {
                        self.check_async_capture(&info.param_infos, &c.args);
                    }
                    return function_call_return_type(&info);
                }

                // Indirect call through a function-type local variable.
                if let Some(var_info) = self.symbols.lookup_var(&c.callee).cloned()
                    && let Type::Fn(param_types, ret) = var_info.ty
                {
                    if param_types.len() != c.args.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "function value `{}` takes {} argument(s) but {} were supplied",
                                    c.callee,
                                    param_types.len(),
                                    c.args.len()
                                ),
                            )
                            .with_label(Label::primary(c.span, "wrong number of arguments")),
                        );
                    }
                    self.check_value_call_args(&param_types, &c.args);
                    return *ret;
                }

                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find function `{}`", c.callee),
                    )
                    .with_label(Label::primary(c.span, "not found in this scope")),
                );
                Type::Void
            }
            Expr::FieldAccess(obj, field_name, span) => {
                let obj_ty = self.check_expr(obj);
                self.resolve_field(&obj_ty, field_name, *span, true)
            }
            Expr::MethodCall(m) => {
                // `.` is instance member access; module items use `::`. Using
                // `math.add(..)` on a module is an error that points at `::`.
                if let Expr::Var(name, _) = &m.object
                    && self.symbols.lookup_var(name).is_none()
                    && self.symbols.lookup_module(name).is_some()
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("`{name}` is a module; use `::` to access its items"),
                        )
                        .with_label(Label::primary(m.span, "module accessed with `.`"))
                        .with_help(format!(
                            "write `{name}::{method}(...)` instead of `{name}.{method}(...)`",
                            method = m.method
                        )),
                    );
                    return Type::Void;
                }
                let obj_ty = self.check_expr(&m.object);
                if let Some(ret) = self.check_option_result_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_concurrency_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_frozen_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_map_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_frozen_map_method_call(&obj_ty, m) {
                    return ret;
                }
                self.check_task_method_call(&obj_ty, m);

                self.resolve_method(&obj_ty, &m.method, &m.args, m.span)
            }
            Expr::StaticCall(s) => {
                self.resolve_static_call(&s.class, &s.type_args, &s.method, &s.args, s.span)
            }
            Expr::StaticField(s) => self.resolve_static_field_read(&s.class, &s.field, s.span),
            Expr::New(n) => self.check_new(n),
            Expr::ObjectLiteral(o) => self.check_object_literal(o),
            Expr::Await(a) => {
                let awaited_ty = self.check_expr(&a.expr);
                if !self.current_async_context {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0801,
                            "`await` can only be used inside an async function",
                        )
                        .with_label(Label::primary(
                            a.span,
                            "`await` used in a non-async function",
                        ))
                        .with_help("make the enclosing function `async`"),
                    );
                    return Type::Void;
                }
                match awaited_ty {
                    Type::Generic(name, mut args)
                        if (name == "Future" || name == "Task") && args.len() == 1 =>
                    {
                        args.remove(0)
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0803,
                                format!("cannot await value of type `{}`", type_name(&other)),
                            )
                            .with_label(Label::primary(a.expr.span(), "expected an awaitable"))
                            .with_help(
                                "await only `Task<T>` values returned by async functions or `Future<T>` runtime APIs",
                            ),
                        );
                        Type::Void
                    }
                }
            }
            Expr::Select(s) => {
                self.check_select(s);
                Type::Void
            }
            Expr::Print(arg, _, _) => {
                self.check_expr(arg);
                Type::Void
            }
            Expr::Ternary(t) => {
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
                let then_ty = self.check_expr(&t.then_expr);
                let else_ty = self.check_expr(&t.else_expr);
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
            Expr::Range(r) => self.check_range(r),
            Expr::Lambda(l) => self.check_lambda(l),
            Expr::Match(m) => self.check_match_expr(m),
            Expr::TryPropagate(inner, span) => self.check_try_propagate(inner, *span),
            Expr::ArrayLiteral(elements, span) => self.check_array_literal(elements, *span),
            Expr::Index(arr, index, span) => self.check_index(arr, index, *span),
        }
    }

    pub(super) fn check_range(&mut self, range: &RangeExpr) -> Type {
        // A range is a first-class `Range<i64>` value. Its bounds are checked
        // normally; a nested range bound surfaces as a non-`i64` bound below.
        let start_ty = self.check_expr(&range.start);
        let end_ty = self.check_expr(&range.end);

        if start_ty != Type::I64 || end_ty != Type::I64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "range bounds must be `i64`, found `{}` and `{}`",
                        type_name(&start_ty),
                        type_name(&end_ty)
                    ),
                )
                .with_label(Label::primary(range.span, "range bounds must be `i64`")),
            );
        }

        range_type()
    }
}

/// Test helper: lex+parse+type-check `source`, returning its diagnostics.
#[cfg(test)]
pub(super) fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = Lexer::new(source).tokenize().expect("lexing failed");
    let (program, parse_errors) = Parser::new(tokens).parse();
    assert!(
        parse_errors.is_empty(),
        "unexpected parse errors: {parse_errors:?}"
    );

    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    checker.errors
}
