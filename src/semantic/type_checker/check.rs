//! The `check_*` type-checking methods (extracted from `mod.rs`). `check_program`
//! stays `pub` (the entry point); the rest are `pub(super)`. As a child module
//! these reach `TypeChecker`'s private fields/methods.

mod effects;
mod statements;

use crate::qualified_name::QualifiedName;
use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
#[cfg(test)]
use crate::lexer::Lexer;
#[cfg(test)]
use crate::parser::Parser;
use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::symbols::*;

use super::*;

#[cfg(test)]
thread_local! {
    static ARRAY_CONSTRUCTOR_ARGUMENT_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LOCAL_INTERFACE_METHOD_RECORDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[willow_continuations::checker]
impl TypeChecker {
    /// Async Rust adapters create a foreign wait even when their returned task
    /// is not immediately awaited. Seed this at the call site so ordinary
    /// async-constructor effect masking cannot erase the foreign wait.
    pub(super) fn check_foreign_call(&mut self, span: Span, is_async: bool) {
        self.record_direct_lock_effect(span, "extern rust", LockEffectKind::Suspend);
        if is_async && !self.local.current_async_context {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0801,
                    "an async Rust bridge can only be called inside an async function",
                )
                .with_label(Label::primary(
                    span,
                    "async Rust call in a non-async function",
                ))
                .with_help("make the enclosing function `async` and await the returned task"),
            );
        }
    }

    /// Introduce a local binding (let, parameter, loop variable, pattern
    /// binding, ...), rejecting the reserved builtin names first.
    ///
    /// `recover()` is not an ordinary function: whether it lowers to a runtime
    /// call or to a constant `None` depends on the enclosing defer, and both
    /// the checker and the backend dispatch it by callee name. A local binding
    /// or parameter named `recover` would therefore be silently ignored at the
    /// call site and compile to the builtin instead of the bound value, so the name is
    /// reserved at every binding site the same
    /// way a top-level `fn recover` is.
    pub(super) fn define_var(&mut self, name: String, info: VarInfo) {
        if name == "recover" {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0351,
                    "`recover` is a reserved builtin function",
                )
                .with_label(Label::primary(
                    info.declaration_span,
                    "cannot bind `recover` as a variable or parameter",
                ))
                .with_help(
                    "rename this binding; `recover()` always refers to the panic-recovery builtin",
                ),
            );
        }
        if self.capture_call_sites {
            self.analysis_symbols.declarations.push(
                crate::semantic::analysis_symbols::Declaration::new(
                    &name,
                    if info.is_param {
                        "parameter"
                    } else {
                        "binding"
                    },
                    info.declaration_span,
                    Some(info.ty.clone()),
                ),
            );
        }
        self.symbols.define_var(name, info);
    }

    /// Report E2003 if `name` (a local declaration) collides with an imported
    /// name (a module access name or a directly imported item).
    pub(super) fn check_local_decl_collision_uncached(&mut self, name: &str, span: Span) {
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

    /// A `const` and a function of the same name in one file share the item
    /// namespace. Codegen reports the clash only once checking succeeds, and
    /// registration here is last-wins, so a use would be checked against the
    /// wrong declaration first. Keep the first one and report the second.
    pub(super) fn reject_constant_redeclaration_uncached(&mut self, f: &FunctionDecl) -> bool {
        let Some(first) = self.symbols.lookup_func(&f.name) else {
            return false;
        };
        let first_span = first.declaration_span;
        if first_span == f.span
            || first_span.file_id != f.span.file_id
            || (first.constant.is_none() && f.constant.is_none())
        {
            return false;
        }
        let kind = |constant: bool| if constant { "constant" } else { "function" };
        let mut earlier = (first_span, kind(first.constant.is_some()));
        let mut later = (f.span, kind(f.constant.is_some()));
        // An imported module's items are pre-registered last-wins, so the
        // registered one may be the later declaration.
        if earlier.0.start > later.0.start {
            std::mem::swap(&mut earlier, &mut later);
        }
        let ((first_span, first_kind), (second_span, second_kind)) = (earlier, later);
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0706,
                format!(
                    "{second_kind} `{}` is already declared as a {first_kind}",
                    f.name
                ),
            )
            .with_label(Label::primary(second_span, "second declaration"))
            .with_label(Label::secondary(first_span, "first declared here"))
            .with_help("rename one of them"),
        );
        true
    }

    /// Check the ENTRY program: the file named on the command line.
    ///
    /// Its `main` is the process entry point, which is the one body allowed to
    /// run off the end of a `Result<void, E>`. Use
    /// [`Self::check_module_program`] for an imported module, whose `main`, if
    /// it has one, is an ordinary function.
    pub fn check_program(&mut self, program: &Program) {
        if self.array_abi_files.is_empty() && !self.source_imports_checked {
            // Standalone semantic clients have no module loader. They consume
            // the same source declaration, with an unspellable private route.
            static BUNDLED: std::sync::OnceLock<Program> = std::sync::OnceLock::new();
            let bundled = BUNDLED.get_or_init(|| {
                let tokens = crate::lexer::Lexer::with_file_id(
                    include_str!("../../../std/collections.wi"),
                    crate::diagnostics::FileId(u32::MAX),
                )
                .tokenize()
                .expect("bundled source lexes");
                let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty(), "bundled source parses");
                crate::stdlib_schema::attach_array_storage(&mut program);
                program
            });
            self.register_module_type_signatures(
                "$std::collections",
                "<std>/collections.wi",
                bundled,
            );
        }
        self.entry_program = true;
        self.check_program_items(program);
    }

    /// Check an IMPORTED MODULE's body with a checker of its own.
    ///
    /// Identical to [`Self::check_program`] except that nothing here is the
    /// entry point. The caller is responsible for the module's scope — prelude,
    /// the modules IT imports, its own item imports — exactly as
    /// `typecheck_phase` builds the entry scope, because a module sees its own
    /// imports and not the entry file's.
    pub fn check_module_program(&mut self, program: &Program) {
        self.entry_program = false;
        self.check_program_items(program);
    }

    fn check_program_items(&mut self, program: &Program) {
        if self.field_writes.is_none() {
            self.field_writes = Some(std::rc::Rc::new(
                crate::semantic::field_writes::FieldWrites::from_program(program),
            ));
        }
        self.register_instantiation_types(&program.instantiation_types);
        self.prepare_precheck(program);
        // Generic declarations are templates and never enter the ordinary
        // signature-registration pass. Index all declarations first so a
        // template cannot silently replace another template or share a name
        // with a concrete declaration that survives specialization.
        let mut declarations = HashMap::new();
        for item in &program.items {
            let (name, span, kind, generic) = match item {
                Item::Function(f) => (
                    &f.name,
                    f.span,
                    if f.constant.is_some() {
                        "constant"
                    } else {
                        "function"
                    },
                    !f.type_params.is_empty(),
                ),
                Item::Class(c) => (&c.name, c.span, "class", !c.type_params.is_empty()),
                Item::Enum(e) => (&e.name, e.span, "enum", !e.type_params.is_empty()),
                Item::Interface(i) => (&i.name, i.span, "interface", !i.type_params.is_empty()),
            };
            if matches!(name.as_str(), "forall" | "exists") {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2631,
                        "forall and exists are reserved quantifier names",
                    )
                    .with_label(Label::primary(span, "reserved declaration name")),
                );
            }
            if let Some(&(first_span, first_kind, first_generic)) = declarations.get(name) {
                self.cached_duplicate_declaration(
                    name,
                    (span, kind, generic),
                    (first_span, first_kind, first_generic),
                );
            } else {
                declarations.insert(name, (span, kind, generic));
            }
            match item {
                Item::Function(f) if !f.type_params.is_empty() => {
                    if let Some(attribute) = f
                        .effect_contracts
                        .iter()
                        .find(|a| a.kind == EffectContractKind::Verified)
                    {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E2631,
                                "UNKNOWN: @verified requires a concrete top-level function",
                            )
                            .with_label(Label::primary(
                                attribute.span,
                                "generic verification is unsupported",
                            )),
                        );
                    }
                    self.generics.functions.insert(
                        f.name.clone(),
                        self.generics.registry.borrow_mut().intern_function(f),
                    );
                }
                Item::Class(c) if !c.type_params.is_empty() => {
                    self.generics.classes.insert(
                        c.name.clone(),
                        self.generics.registry.borrow_mut().intern_class(c),
                    );
                }
                _ => {}
            }
        }

        self.marker_contracts.clear();
        self.marker_contract_proven.clear();
        self.register_std_imports(&program.imports);
        // Looping sync helpers indexed by `Class::method`, so a call through a
        // typed non-`self` receiver (`obj.heavy()`) in a task context is flagged
        // E0810 — the AST-only ConcurrencyAnalyzer cannot resolve the receiver
        // type.
        // Helper effects depend on checked call resolution. Collect their
        // diagnostic sites while checking; solve and replay them afterward.
        self.task_method_calls.clear();

        // Pass 0: enum identities. Normalizing a written type canonicalizes a
        // bare enum name by lookup, so every enum must be findable before any
        // declaration's types are normalized: a class field, interface
        // signature or payload may name an enum declared later in the file. The reserved name
        // is left to pass 1's diagnostic.
        for item in &program.items {
            if let Item::Enum(e) = item
                && e.name != "PanicInfo"
            {
                self.predeclare_enum(e);
            }
        }

        // Pass 1: register class shapes, enum declarations, and interfaces.
        // Interfaces share the top-level namespace with classes/enums/functions
        // and must be registered before class conformance is validated.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.check_local_decl_collision(&c.name, c.span);
                    if c.name == "PanicInfo" {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0351,
                                "`PanicInfo` is a reserved runtime type",
                            )
                            .with_label(Label::primary(c.span, "cannot redeclare `PanicInfo`")),
                        );
                        continue;
                    }
                    if !c.type_params.is_empty() {
                        continue;
                    }
                    self.precheck_declaration(item);
                }
                Item::Enum(e) => {
                    self.check_local_decl_collision(&e.name, e.span);
                    if e.name == "PanicInfo" {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0351,
                                "`PanicInfo` is a reserved runtime type",
                            )
                            .with_label(Label::primary(e.span, "cannot redeclare `PanicInfo`")),
                        );
                        continue;
                    }
                    self.precheck_declaration(item);
                }
                Item::Interface(i) => {
                    self.check_local_decl_collision(&i.name, i.span);
                    if i.name == "PanicInfo" {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0351,
                                "`PanicInfo` is a reserved runtime type",
                            )
                            .with_label(Label::primary(i.span, "cannot redeclare `PanicInfo`")),
                        );
                        continue;
                    }
                    self.precheck_declaration(item);
                }
                _ => {}
            }
        }

        self.signature_values.borrow_mut().clear();
        // Pass 2: register all top-level function signatures
        for item in &program.items {
            if let Item::Function(f) = item {
                self.check_local_decl_collision(&f.name, f.span);
                if f.name == "recover" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0351,
                            "`recover` is a reserved builtin function",
                        )
                        .with_label(Label::primary(f.span, "cannot redeclare `recover`")),
                    );
                    continue;
                }
                if self.cached_constant_collision(f) {
                    continue;
                }
                if !f.type_params.is_empty() {
                    continue;
                }
                let info = self.normalized_signature(f);
                self.symbols.define_func(f.name.clone(), info);
            }
        }

        // Build the complete same-file callable index before checking any
        // body. Calls from an earlier definition to a later one must take part
        // in the lock-effect fixpoint just like backward calls do.
        self.prepare_lock_effect_analysis(program);

        if let Some((queries, unit)) = &self.declaration_queries {
            match queries.freeze(*unit, &self.symbols) {
                Ok(symbols) => self.symbols = symbols,
                Err(error) => {
                    self.body_query_error = Some(error);
                    return;
                }
            }
        }

        // Pass 3: check bodies
        for item in &program.items {
            match item {
                Item::Function(f) if f.type_params.is_empty() => self.check_function(f),
                Item::Function(_) => {}
                Item::Class(c) if c.type_params.is_empty() => self.check_class(c),
                Item::Class(_) => {}
                Item::Enum(e) => self.check_enum(e), // validate payload types
                Item::Interface(i) => {
                    self.analysis_symbols
                        .used_imports
                        .extend(i.default_import_uses.iter().cloned());
                    self.check_interface(i);
                }
            }
        }

        if let Some(file) = program.items.iter().find_map(|item| match item {
            Item::Function(f) if !f.type_params.is_empty() => Some(f.span.file_id),
            Item::Class(c) if !c.type_params.is_empty() => Some(c.span.file_id),
            Item::Interface(i)
                if !i.type_params.is_empty()
                    && !i.template_checked
                    && (!i.extends.is_empty()
                        || i.methods.iter().any(|m| !m.effect_contracts.is_empty())) =>
            {
                Some(i.span.file_id)
            }
            _ => None,
        }) {
            self.validate_generic_templates(file, program);
        }
        self.finish_effect_analysis(program);
        // Ownership-transfer failures retain resolved types and identities.
        // Keep those errors and also explain a statically visible wait cycle.
        // Other type/flow errors make the wait proof unavailable.
        if self.effect_graph_only.is_none()
            && self.entry_program
            && self
                .errors
                .iter()
                .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2408)
        {
            let result = crate::semantic::permanent_wait::check(
                program,
                &self.expr_types,
                &self.static_call_classes,
                &self.builtin_calls,
                &self.analysis_calls,
            );
            for diagnostic in result.diagnostics {
                self.push(diagnostic);
            }
        }
        self.warn_unused_imports(program);
    }

    /// A static call (not a static field read). `expected` is the contextual
    /// result type: it reaches the arguments of a generic function and of a
    /// lock constructor, so `Mutex::new(Map::new())` expected as
    /// `Mutex<Map<i64, i64>>` types its argument as `Map<i64, i64>`.
    pub(super) fn check_static_call(
        &mut self,
        s: &StaticCallExpr,
        context: CallContext<'_>,
    ) -> Type {
        if let Some(owner) = self.resolve_static_call_class_quiet(&s.class)
            && let Some((name, info)) = self.native_member(&owner, &s.method, false)
        {
            self.check_type_visibility(&owner, s.span);
            self.static_call_classes
                .insert(s.id, TypeId::from_source_name(&owner));
            if !s.type_args.is_empty() {
                self.generic_error(s.span, "native functions do not accept type arguments");
            }
            return self.check_native_member(s.id, s.span, name, info, &s.args, false);
        }
        // Constructor syntax supplies a class argument from the expected array
        // when omitted, then uses the declaration's ordinary static dispatch.
        if self
            .resolve_static_call_class_quiet(&s.class)
            .as_deref()
            .and_then(B::from_name)
            == Some(B::Array)
            && self
                .generics
                .classes
                .contains_key("$std::collections::Array")
        {
            if self
                .resolve_static_call_class_name(&s.class, s.span)
                .is_none()
            {
                return Self::error_type();
            }
            self.check_collection_type_imported(B::Array.source_name(), s.span);
            if context.checked
                && (!s.type_args.is_empty() || !matches!(context.expected, Some(Type::Array(_))))
            {
                return self.reuse_checked_type(s.id);
            }
            let inferred;
            let mut checked_argument = None;
            let arguments = if s.type_args.is_empty() {
                inferred = match context.expected {
                    Some(Type::Array(element)) => vec![element.as_ref().clone()],
                    _ => {
                        // Infer from a declared T parameter, then reuse that check
                        // during ordinary argument validation. Nested constructors
                        // must not recursively check their value twice.
                        let template = &self.generics.classes["$std::collections::Array"];
                        let argument = template.methods.iter()
                            .find(|method| method.name == s.method && method.is_static())
                            .and_then(|method| method.params.iter().zip(&s.args).find_map(|(param, arg)| {
                                (matches!(&param.ty, Type::Named(name) if template.type_params.contains(name))
                                    && param.mode == ParamMode::Value && arg.mode == CallArgMode::Value)
                                    .then_some(arg)
                            }));
                        if let Some(argument) = argument {
                            #[cfg(test)]
                            ARRAY_CONSTRUCTOR_ARGUMENT_CHECKS
                                .with(|count| count.set(count.get() + 1));
                            let ty = self.check_expr(&argument.expr);
                            checked_argument = Some((argument.expr.id(), ty.clone()));
                            vec![ty]
                        } else {
                            vec![Type::Void]
                        }
                    }
                };
                &inferred
            } else {
                &s.type_args
            };
            if arguments == &[Type::Void] && s.type_args.is_empty() {
                // Keep an unresolved empty container as a semantic placeholder.
                // Its declaration still owns argument modes, arity and result;
                // only concrete contexts request executable specializations.
                let template = self.generics.classes["$std::collections::Array"].clone();
                let Some(method) = template
                    .methods
                    .iter()
                    .find(|method| method.name == s.method)
                else {
                    self.generic_error(s.span, "constructor is not declared by Array");
                    return Self::error_type();
                };
                if !method.public() || !method.is_static() || method.constraint.is_some() {
                    self.generic_error(
                        s.span,
                        "constructor is not available without a known element type",
                    );
                    return Self::error_type();
                }
                let bindings = template
                    .type_params
                    .iter()
                    .cloned()
                    .zip(arguments.iter().cloned())
                    .collect();
                let params: Vec<_> = method
                    .params
                    .iter()
                    .map(|parameter| ParamInfo {
                        ty: crate::semantic::generic_inference::substitute(
                            &parameter.ty,
                            &bindings,
                        ),
                        mode: parameter.mode.clone(),
                        sending: parameter.sending,
                        span: parameter.span,
                        type_span: parameter.type_span,
                    })
                    .collect();
                self.check_call_argument_count(&s.method, params.len(), s.args.len(), s.span);
                if let Some((id, ty)) = &checked_argument {
                    self.prechecked_array_args.insert(*id, ty.clone());
                }
                self.check_call_args_against_param_infos(&params, &s.args, context.checked);
                if let Some((id, _)) = checked_argument {
                    self.prechecked_array_args.remove(&id);
                }
                self.check_async_call_captures(&params, &s.args, method.is_async());
                self.record_method_use(&s.method, "method", s.span, method.span, &s.args);
                let result =
                    crate::semantic::generic_inference::substitute(&method.return_type, &bindings);
                let result = self.normalize_type(&result, s.span);
                return if method.is_async() {
                    B::Task.apply(vec![result])
                } else {
                    result
                };
            }
            let Some(Type::Named(ref owner)) =
                self.instantiate_class("$std::collections::Array", arguments, s.span)
            else {
                return Self::error_type();
            };
            self.generics.objects.insert(s.id, owner.clone());
            self.static_call_classes
                .insert(s.id, TypeId::from_source_name(owner));
            if let Some((id, ty)) = &checked_argument {
                self.prechecked_array_args.insert(*id, ty.clone());
            }
            let result = self.resolve_static_call(
                owner,
                &[],
                &s.method,
                &s.args,
                (s.id, s.span, s.method_span),
                context,
            );
            if let Some((id, _)) = checked_argument {
                self.prechecked_array_args.remove(&id);
            }
            self.record_lock_effect_call(
                FunctionId::method(TypeId::from_source_name(owner), &s.method),
                s.span,
            );
            return result;
        }
        let generic_name = QualifiedName::join(&s.class, &s.method);
        if self.generics.functions.contains_key(&generic_name)
            && let Some(ty) = self.check_generic_call_parts(
                &generic_name,
                &s.type_args,
                &s.args,
                s.span,
                s.id,
                context,
            )
        {
            return ty;
        }
        // Only a lock constructor can use a result hint in the remaining
        // static resolvers. All ordinary argument checks already ran.
        if context.checked {
            let lock_context = s.method == "new"
                && s.type_args.is_empty()
                && matches!(context.expected, Some(Type::Generic(name, args)) if args.len() == 1
                    && matches!(B::from_name(name), Some(B::Mutex | B::RwLock | B::BlockingCell | B::BlockingRwCell))
                    && self.resolve_static_call_class_quiet(&s.class).as_deref() == Some(name));
            if !lock_context {
                return self.reuse_checked_type(s.id);
            }
        }
        self.record_static_builtin_lock_effect(s);
        let callee = self.static_method_effect_id(&s.class, &s.method);
        let result = self.resolve_static_call(
            &s.class,
            &s.type_args,
            &s.method,
            &s.args,
            (s.id, s.span, s.method_span),
            context,
        );
        if let Some(callee) = callee {
            self.record_lock_effect_call(callee, s.span);
        }
        result
    }

    pub(super) fn check_or_reuse_expr(&mut self, expr: &Expr, checked: bool) -> Type {
        if checked && let Some(ty) = self.expr_types.get(&expr.id()).cloned() {
            self.reuse_check_failure(super::body::FailedCheck::Value(expr.id()));
            return ty;
        }
        self.check_expr(expr)
    }

    pub(super) fn check_expr(&mut self, expr: &Expr) -> Type {
        let generation = self.error_generation;
        let mut ty = self.check_expr_inner(expr);
        if self.error_generation == generation
            && matches!(&ty, Type::Generic(name, _) if matches!(B::from_name(name), Some(B::Option | B::Result)))
            && self.report_constructor_holes(expr, &ty)
        {
            ty = Self::error_type();
        }
        self.finish_checked_expr(expr, ty, generation)
    }

    /// Check a value whose omitted enum parameters a sibling branch, the
    /// enclosing operator or `?` may still supply. The enclosing inference
    /// boundary reports whatever stays unresolved.
    pub(super) fn check_branch_value(&mut self, expr: &Expr) -> Type {
        let generation = self.error_generation;
        let ty = self.check_expr_inner(expr);
        self.finish_checked_expr(expr, ty, generation)
    }

    /// Check a value an unannotated lambda returns. The call site may still
    /// complete the inferred type of a call argument; any other lambda's type
    /// is final here.
    pub(super) fn check_lambda_return_value(&mut self, expr: &Expr) -> Type {
        if self.local.lambda_holes_deferred {
            self.check_branch_value(expr)
        } else {
            self.check_expr(expr)
        }
    }

    /// Report omitted Option/Result parameters left after branch merging on
    /// each constructor leaf. Each branch node is visited once.
    fn report_constructor_holes(&mut self, expr: &Expr, ty: &Type) -> bool {
        let mut reported = false;
        let mut pending = vec![expr];
        while let Some(expr) = pending.pop() {
            match expr {
                Expr::Ternary(t) => {
                    pending.push(&t.else_expr);
                    pending.push(&t.then_expr);
                }
                Expr::Match(m) => {
                    pending.extend(m.arms.iter().rev().filter_map(|arm| match &arm.body {
                        MatchBody::Expr(expr) => Some(expr.as_ref()),
                        _ => None,
                    }))
                }
                _ => {
                    if let Some(diag) =
                        self.unresolved_generic_enum_diagnostic(expr, ty, expr.span(), "x")
                    {
                        self.push(diag);
                        reported = true;
                    }
                }
            }
        }
        reported
    }

    /// The common tail of [`Self::check_expr`], shared with contextual checks
    /// that type the node themselves.
    pub(super) fn finish_checked_expr(&mut self, expr: &Expr, ty: Type, generation: usize) -> Type {
        // Imported signatures retain generic class applications until a value
        // reaches this checker. Materialize the class before recording its type
        // or letting a parent expression resolve fields/methods on the value.
        let ty = if let Type::Generic(name, _) = &ty
            && self.generics.classes.contains_key(name)
        {
            self.normalize_type(&ty, expr.span())
        } else {
            ty
        };
        self.record_checked_expr(expr, ty, generation)
    }

    pub(super) fn record_static_target(&mut self, expr: &Expr) {
        // What the written class turned out to name, per call site: HIR lowering
        // rewrites `Level::High` to the build-wide identity `signal::Level::High`
        // with this, because the node's type already carries that identity and
        // the enum tables answer to nothing else.
        let written_class = match expr {
            Expr::StaticCall(s) => Some(s.class.as_str()),
            Expr::StaticField(s) => Some(s.class.as_str()),
            _ => None,
        };
        if let Some(written) = written_class {
            if self.static_call_classes.contains_key(&expr.id()) {
                return;
            }
            let resolved = self
                .resolve_static_call_class_quiet(written)
                .unwrap_or_else(|| written.to_owned());
            self.static_call_classes.insert(
                expr.id(),
                crate::semantic::ids::TypeId::from_source_name(&resolved),
            );
        }
    }

    /// An exact, successful result needs no new contextual inference.
    pub(super) fn checked_as(&self, id: ExprId, expected: &Type) -> bool {
        self.expr_types.get(&id) == Some(expected)
            && !self
                .local
                .failed_checks
                .contains(&super::body::FailedCheck::Value(id))
    }

    pub(super) fn record_check_failure(&mut self, part: super::body::FailedCheck, failed: bool) {
        if failed {
            self.local.failed_checks.insert(part);
        } else if !self.local.failed_checks.is_empty() {
            self.local.failed_checks.remove(&part);
        }
    }

    pub(super) fn reuse_check_failure(&mut self, part: super::body::FailedCheck) {
        if self.local.failed_checks.contains(&part) {
            // Preserve error recovery in enclosing expressions without emitting
            // another copy of the diagnostic or walking the child again.
            self.error_generation += 1;
        }
    }

    pub(super) fn reuse_checked_type(&mut self, id: ExprId) -> Type {
        self.reuse_check_failure(super::body::FailedCheck::Value(id));
        self.expr_types[&id].clone()
    }

    pub(super) fn record_checked_expr(&mut self, expr: &Expr, ty: Type, generation: usize) -> Type {
        self.record_callable_join(expr, &ty);
        if self.error_generation != 0 {
            self.record_check_failure(
                super::body::FailedCheck::Value(expr.id()),
                self.error_generation != generation,
            );
        }
        let ty = if ty == Type::Void && self.error_generation != generation {
            Self::error_type()
        } else {
            ty
        };
        self.record_static_target(expr);
        // Record the authoritative expression type for downstream consumers
        // (HIR lowering, typed HIR lowering): keyed by node ID, so a pass that rebuilds
        // or duplicates syntax cannot inherit another node's entry the way two
        // nodes sharing a span could.
        self.expr_types.insert(expr.id(), ty.clone());
        self.record_resolved_call(expr);
        self.record_symbol_use(expr);
        ty
    }

    /// The parser cannot distinguish uppercase class properties from enum variants.
    /// A known class makes a bare member a field; parentheses still mean a call.
    /// Classify by receiver only, so the field resolver walks inheritance once.
    pub(super) fn is_static_field_call(&self, call: &StaticCallExpr) -> bool {
        call.is_bare_member()
            && (self
                .symbols
                .lookup_module_func(&call.class, &call.method)
                .is_some_and(|info| info.constant.is_some())
                || self
                    .resolve_static_call_class_quiet(&call.class)
                    .is_some_and(|name| self.symbols.lookup_class(&name).is_some()))
    }

    /// Record resolution at the checked expression, while lexical bindings and
    /// receiver types are still available. Unknown is explicit for panic safety.
    fn record_resolved_call(&mut self, expr: &Expr) {
        use crate::semantic::intrinsics::BuiltinMethod;
        let mut inline_callback = false;
        let resolved = self
            .analysis_calls
            .get(&expr.id())
            .copied()
            .flatten()
            .filter(|id| {
                id.is_method_of("$output")
                    || matches!(expr, Expr::New(_))
                    || self
                        .symbols
                        .native_function(&id.to_string())
                        .is_some_and(|info| info.native.is_some())
            });
        let mut targets = match expr {
            _ if resolved.is_some() => vec![resolved.unwrap()],
            // Constant reads inline their value. Keep a string constant's
            // resolved identity outside AI capture too, so allocation proofs
            // distinguish it from a shadowing local without resolving again.
            Expr::Var(name, _, id)
                if self.symbols.lookup_func(name).is_some_and(|info| {
                    self.capture_call_sites || matches!(info.constant, Some(ConstValue::String(_)))
                }) && self.symbols.lookup_var(name).is_none() =>
            {
                self.analysis_calls
                    .insert(*id, Some(FunctionId::free_from_source_name(name)));
                return;
            }
            // Internally generated output nodes use the same trusted runtime edge.
            Expr::Print(_, newline, _, _) => vec![FunctionId::method(
                TypeId::local("$output"),
                if *newline { "println" } else { "print" },
            )],
            // A variant constructor (`Err(e)` inferred without context) is a
            // value, like the contextual form that never records a call.
            Expr::Call(call) if self.enum_variant_resolutions.contains_key(&call.id) => return,
            // Quantifiers bind a predicate in this callable; there is no call or closure.
            Expr::Call(call) if matches!(call.callee.as_str(), "forall" | "exists") => return,
            Expr::Call(call) => {
                if self.symbols.lookup_var(&call.callee).is_some() {
                    Vec::new()
                } else {
                    vec![FunctionId::free_from_source_name(&call.callee)]
                }
            }
            // A builtin method has no Willow body but a known lowering, so it
            // resolves to its intrinsic target instead of an unknown site.
            Expr::MethodCall(call) if self.is_super_receiver(&call.object) => {
                self.super_call_target(call).into_iter().collect()
            }
            Expr::MethodCall(call) => self
                .expr_types
                .get(&call.object.id())
                .map(|ty| {
                    let mut targets = if matches!(ty, Type::Array(_) | Type::FixedArray(..))
                        && let Some(&owner) = self.static_call_classes.get(&call.id)
                    {
                        vec![FunctionId::method(owner, &call.method)]
                    } else {
                        self.method_effect_ids(ty, &call.method)
                    };
                    if targets.is_empty() {
                        match self.builtin_calls.effect_method(&call.id) {
                            Some(BuiltinMethod::Intrinsic(id)) => targets.push(id.function_id()),
                            Some(BuiltinMethod::Adt(method)) => {
                                targets.extend(method.target());
                                inline_callback = method.callback();
                            }
                            None => {}
                        }
                    }
                    targets
                })
                .unwrap_or_default(),
            // Qualified enum construction is a value too. Its representation
            // supplies allocation evidence; it has no missing callable body, so
            // it never enters the call graph. AI capture still needs the
            // resolved variant identity to attach its allocation witness.
            Expr::StaticCall(call) if self.is_static_enum_variant(&call.class, &call.method) => {
                if self.capture_call_sites || self.local.in_constructor {
                    let owner = self
                        .static_call_classes
                        .get(&call.id)
                        .copied()
                        .unwrap_or_else(|| TypeId::from_source_name(&call.class));
                    self.analysis_calls
                        .insert(expr.id(), Some(FunctionId::method(owner, &call.method)));
                }
                return;
            }
            // Bare class properties/module constants do not invoke a method.
            // Their initializer dependencies are recorded by the field resolver.
            Expr::StaticCall(call) if self.is_static_field_call(call) => return,
            Expr::StaticCall(call) => vec![
                self.static_method_effect_id(&call.class, &call.method)
                    .unwrap_or_else(|| {
                        FunctionId::method(
                            self.static_call_classes
                                .get(&call.id)
                                .copied()
                                .unwrap_or_else(|| TypeId::from_source_name(&call.class)),
                            &call.method,
                        )
                    }),
            ],
            Expr::New(call) => vec![FunctionId::method(
                TypeId::from_source_name(
                    &self
                        .resolve_static_call_class_quiet(&call.class_name)
                        .unwrap_or_else(|| call.class_name.clone()),
                ),
                "init",
            )],
            _ => return,
        };
        self.record_indirect_uses(expr, &targets);
        // Constructors always record them: E0842 refinement looks up each
        // call's panic fact by expression id.
        if self.capture_call_sites || self.local.in_constructor {
            // Keep only a unique compiler-resolved target. Polymorphic sites
            // remain explicit unknowns, without copying dispatch fan-out per site.
            self.analysis_calls.insert(
                expr.id(),
                if targets.len() == 1 {
                    Some(targets[0])
                } else {
                    None
                },
            );
        }
        let Some(caller) = self.local.current_effect_callable else {
            return;
        };
        // Preserve the typed receiver for build-wide dispatch. Direct static
        // calls and constructors deliberately do not enter this set.
        let virtual_call = if let Expr::MethodCall(call) = expr
            && !self.is_super_receiver(&call.object)
        {
            self.expr_types.get(&call.object.id()).and_then(|ty| {
                let (Type::Named(name) | Type::Generic(name, _)) = ty else {
                    return None;
                };
                let name = if name == "Self" && self.local.self_interface.is_some() {
                    self.local.current_class.as_ref().unwrap_or(name)
                } else {
                    name
                };
                let owner = self
                    .symbols
                    .lookup_interface(name)
                    .map(|info| &info.name)
                    .or_else(|| self.symbols.lookup_class(name).map(|info| &info.name))?;
                Some(FunctionId::method(
                    TypeId::from_source_name(owner),
                    &call.method,
                ))
            })
        } else {
            None
        };
        use crate::semantic::call_graph::UnsupportedInitialization;
        let unsupported = if let Expr::StaticCall(call) = expr {
            let class = self
                .static_call_classes
                .get(&call.id)
                .map(ToString::to_string)
                .unwrap_or_else(|| call.class.clone());
            self.imported_std_modules
                .get(&class)
                .filter(|module| {
                    // Inspect the borrowed schema, without rebuilding builtin
                    // parameter types at every call site.
                    matches!(crate::stdlib_schema::item(&module.module, &call.method).map(|item| &item.kind),
                        Some(crate::stdlib_schema::StdItemKind::Function { params, .. })
                        if params.iter().any(|ty| matches!(ty, crate::stdlib_schema::StdType::Fn(_, _))))
                })
                .map(|_| UnsupportedInitialization::RuntimeCallback)
        } else if targets.is_empty() {
            Some(if inline_callback {
                UnsupportedInitialization::InlineCallback
            } else if matches!(expr, Expr::Call(_)) {
                UnsupportedInitialization::IndirectCall
            } else if virtual_call.is_some() {
                UnsupportedInitialization::UnresolvedVirtualCall
            } else {
                UnsupportedInitialization::UnresolvedCall
            })
        } else {
            None
        };
        for target in &mut targets {
            *target = self.effect_index.local_target(*target);
        }
        let sites = self.resolved_calls.entry(caller).or_default();
        if targets.is_empty()
            && virtual_call.is_none()
            && !matches!(
                unsupported,
                Some(
                    UnsupportedInitialization::IndirectCall
                        | UnsupportedInitialization::InlineCallback
                )
            )
        {
            sites.unknown_span.get_or_insert(expr.span());
        }
        if let Some(target) = virtual_call {
            sites.virtual_spans.entry(target).or_insert(expr.span());
        } else {
            for &target in &targets {
                sites.call_spans.entry(target).or_insert(expr.span());
            }
        }
        sites.unsupported_initialization.extend(unsupported);
        sites.virtual_calls.extend(virtual_call);
        sites.has_unknown |= targets.is_empty();
        // Keep canonical IDs in typed call-site/dispatch metadata, but connect
        // the unit effect graph to its source-local body IDs. Otherwise a
        // self call in an imported module queries its own unfinished effects.
        sites.targets.extend(targets);
    }

    /// `LIMIT()` where `LIMIT` is a module-level `const`: a
    /// constant is a value, never a callable function, even though it is
    /// registered in the function namespace.
    pub(super) fn reject_constant_call(
        &mut self,
        name: &str,
        info: &FuncInfo,
        args: &[CallArg],
        span: Span,
    ) -> Type {
        for arg in args {
            self.check_expr(&arg.expr);
        }
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "cannot call constant `{name}` of type `{}`",
                    self.diagnostic_type_name(&info.return_type)
                ),
            )
            .with_label(Label::primary(span, "constants are values, not functions"))
            .with_help(format!("use `{name}` without parentheses")),
        );
        Self::error_type()
    }

    /// The class and type arguments a `new` or static call names, unless a
    /// bare static call resolves to the enclosing class's own receiver. When
    /// that class is a generic template, `check_expr_inner` instantiates it
    /// and checks the call against the instance.
    pub(super) fn generic_class_site<'e>(
        &self,
        expr: &'e Expr,
    ) -> Option<(&'e String, &'e Vec<Type>)> {
        match expr {
            Expr::StaticCall(c)
                if c.type_args.is_empty() && self.enclosing_static_receiver(&c.class).is_some() =>
            {
                None
            }
            Expr::StaticCall(c) => Some((&c.class, &c.type_args)),
            _ => None,
        }
    }

    pub(super) fn check_new_with_effects(
        &mut self,
        n: &Construction<'_>,
        class: &str,
        checked: bool,
    ) -> Type {
        let result = self.check_new(n, class, checked);
        if let Type::Named(owner) = &result {
            self.static_call_classes
                .insert(n.id, TypeId::from_source_name(owner));
            self.analysis_calls.insert(
                n.id,
                Some(FunctionId::method(TypeId::from_source_name(owner), "init")),
            );
        }
        if self
            .symbols
            .lookup_class(class)
            .is_some_and(|info| info.constructor.is_some())
        {
            self.record_lock_effect_call(
                FunctionId::method(TypeId::from_source_name(class), "init"),
                n.span,
            );
        }
        result
    }

    fn check_expr_inner(&mut self, expr: &Expr) -> Type {
        if let Some(newline) = self.output_function(expr) {
            return self.check_output_call(expr, newline);
        }
        if let Expr::Var(name, span, _) = expr
            && self.symbols.lookup_var(name).is_none()
            && (self.generics.functions.contains_key(name)
                || self.generics.classes.contains_key(name))
        {
            self.generic_error(
                *span,
                format!(
                    "generic declaration `{name}` cannot be used as a value without instantiation"
                ),
            );
            return Self::error_type();
        }

        // Reject obsolete syntax before specialization can replace the source
        // class name with a monomorphized name or a recovery-only error marker.
        if let Expr::ObjectLiteral(literal) = expr {
            return self.check_object_literal(literal);
        }

        if let Some(construction) = self.construction(expr) {
            return self.check_constructor_call(&construction);
        }
        if let Some((name, type_args, args)) = self.call_form_class(expr) {
            return self.reject_call_form_construction(expr, &name, type_args, args);
        }
        let generic_class = self.generic_class_site(expr);
        if let Some((name, args)) = generic_class
            && let Some(ty) = self.instantiate_class(name, args, expr.span())
        {
            let Type::Named(name) = &ty else {
                return ty;
            };
            self.generics.objects.insert(expr.id(), name.clone());

            let mut concrete = expr.clone();
            match &mut concrete {
                Expr::StaticCall(c) => {
                    c.class = name.clone();
                    c.type_args.clear();
                }
                _ => unreachable!(),
            }
            return self.check_expr_inner(&concrete);
        }

        match expr {
            Expr::Integer(_, _, _) => self.check_expr_expecting(expr, &Type::I64),
            Expr::Char(_, _, _) => Type::Char,
            Expr::Float(_, _, _) => Type::F64,
            Expr::Bool(_, _, _) => Type::Bool,
            Expr::String(_, _, _) => Type::String,
            Expr::Var(name, span, _) => self.check_variable_value(expr, name, span),
            Expr::Binary(_) | Expr::Unary(_) => self.check_operator_tree(expr),
            Expr::Call(c) if matches!(c.callee.as_str(), "forall" | "exists") => {
                self.check_quantifier(c)
            }
            Expr::Call(c) => self.check_named_call(c),
            Expr::FieldAccess(obj, field_name, span, _) => {
                let obj_ty = self.check_expr(obj);
                self.resolve_field(&obj_ty, field_name, *span, true, "read")
            }
            Expr::MethodCall(m) => self.check_method_call_expecting(m, None, false),
            Expr::StaticCall(s) if self.is_static_field_call(s) => {
                self.resolve_static_field_read(&s.class, &s.method, s.span)
            }
            Expr::StaticCall(s) => self.check_static_call(s, CallContext::default()),
            Expr::StaticField(s) => self.resolve_static_field_read(&s.class, &s.field, s.span),
            Expr::New(_) => unreachable!("construction handled above"),
            Expr::ObjectLiteral(o) => self.check_object_literal(o),
            Expr::Await(a) => self.check_await_expr(a),
            Expr::Select(s) => {
                self.record_direct_lock_effect(s.span, "select", LockEffectKind::Suspend);
                self.check_select(s);
                Type::Void
            }
            Expr::Print(arg, newline, _, _) => self.check_print_expr(arg, newline),
            Expr::Ternary(t) => self.check_ternary_expr(t, None, false),
            Expr::Range(r) => self.check_range(r),
            Expr::Lambda(l) => self.check_lambda(l),
            Expr::Match(m) => self.check_match_expr(m, None, false),
            Expr::TryPropagate(inner, span, _) => self.check_try_propagate(inner, *span),
            Expr::ArrayLiteral(elements, _span, _) => self.check_array_literal(elements),
            Expr::Index(arr, index, span, _) => self.check_index(arr, index, *span),
        }
    }

    fn check_variable_value(&mut self, expr: &Expr, name: &str, span: &Span) -> Type {
        if name == "this" {
            self.push_legacy_this_error(*span);
            return Type::Void;
        }
        // Local variable?
        if let Some(info) = self.symbols.lookup_var(name) {
            if matches!(info.ty, Type::Fn(..) | Type::Closure(..)) {
                self.local
                    .capture_flow
                    .reads
                    .insert(*span, info.declaration_span);
                if matches!(info.ty, Type::Closure(..))
                    && self
                        .local
                        .current_capture_declarations
                        .contains(&info.declaration_span)
                {
                    self.local
                        .capture_flow
                        .origins
                        .entry(info.declaration_span)
                        .or_insert_with(|| super::capture_flow::Origin {
                            binding: None,
                            label: std::rc::Rc::new(Label::secondary(
                                info.declaration_span,
                                format!("captured variable `{name}` has task-local function type"),
                            )),
                        });
                }
            }
            if let Some(cause) = self
                .local
                .capture_bindings
                .get(&info.declaration_span)
                .and_then(Option::as_ref)
                .cloned()
            {
                self.local.callable_causes.insert(*span, cause);
            } else {
                self.local.callable_causes.remove(span);
            }
            if let Some((uses, _)) = self.local.inferred_maps.get_mut(&info.declaration_span) {
                uses.push(expr.id());
                self.local.pending_map_uses.insert(expr.id());
            }
            return info.ty.clone();
        }
        // Named function used as a value: `apply(10, double)` where `double: fn(...)`
        if let Some(info) = self.symbols.lookup_func(name) {
            // A module-level `const` is a value of its declared type,
            // inlined by lowering.
            if info.constant.is_some() {
                return info.return_type.clone();
            }
            let params = info.params.clone();
            let ret = info.return_type.clone();
            // An async function has no plain entry point (a call
            // creates its Task), and a builtin is lowered inline at
            // each call site, so neither has an address to take. Builtins are the functions without a
            //
            // source declaration; a few are defined as real functions.
            let is_async = info.is_async;
            let not_addressable = if is_async {
                Some("an async function")
            } else if info.declaration_span == Span::dummy()
                && !crate::semantic::intrinsics::ADDRESSABLE_BUILTINS.contains(&name)
            {
                Some("a builtin function")
            } else {
                None
            };
            if let Some(kind) = not_addressable {
                let args = (0..params.len())
                    .map(|i| format!("a{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let typed = params
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| format!("a{i}: {}", self.diagnostic_type_name(ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1012,
                        format!("`{name}` is {kind} and cannot be used as a function value"),
                    )
                    .with_label(Label::primary(*span, "named here as a value"))
                    .with_help(format!(
                        "wrap the call in a lambda instead: `|{typed}| {name}({args})`"
                    )),
                );
                // The type the call would have, so uses of the
                // value do not report follow-on errors.
                let ret = if is_async {
                    Type::Generic("Task".into(), vec![ret])
                } else {
                    ret
                };
                return Type::Fn(params, Box::new(ret));
            }
            let arity = params.len();
            let ty = Type::Fn(params, Box::new(ret));
            self.record_function_value(
                Some(FunctionId::free_from_source_name(name)),
                arity,
                *span,
                &ty,
            );
            return ty;
        }
        // Give a specialized error for receiver keywords used outside instance methods.
        if name == "self" {
            let diag = if self.local.in_static_method {
                let where_ = self
                    .local
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
                .with_help("static methods have no receiver; use an instance method instead")
            } else if self.local.in_static_initializer {
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
                .with_help("declare the method without `static` to make it an instance method")
            };
            self.push(diag);
            return Type::Void;
        }
        if name == "super" {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0851,
                    "`super` is not a value",
                )
                .with_label(Label::primary(*span, "`super` used as a value"))
                .with_help(
                    "call a base-class method with `super.method(...)`, or read fields through `self`",
                ),
            );
            return Self::error_type();
        }
        if self.report_contextless_variant(name, *span) {
            return Self::error_type();
        }
        if matches!(name, "print" | "println") || self.imported_output_functions.contains_key(name)
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1012,
                    "a polymorphic output function needs a concrete callback signature",
                )
                .with_label(Label::primary(*span, "argument type is not specified"))
                .with_help(format!(
                    "wrap the function in a typed lambda, for example `|value: i64| {name}(value)`"
                )),
            );
            return Self::error_type();
        }
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0350,
                format!("cannot find variable `{}`", name),
            )
            .with_label(Label::primary(*span, "not found in this scope")),
        );
        // The error type suppresses cascades: a use of the missing name
        // must not be judged as some guessed type.
        Self::error_type()
    }

    fn check_quantifier(&mut self, c: &CallExpr) -> Type {
        if c.args.len() != 3 || !c.type_args.is_empty() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2631,
                    "quantifiers require two i64 bounds and one predicate binder",
                )
                .with_label(Label::primary(c.span, "unsupported quantified expression")),
            );
            return Self::error_type();
        }
        self.reject_reference_args(&c.callee, &c.args);
        for bound in &c.args[..2] {
            if self.check_expr(&bound.expr) != Type::I64 {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2631,
                        "quantifier bounds must have type i64",
                    )
                    .with_label(Label::primary(bound.span, "invalid bound")),
                );
            }
        }
        let Expr::Lambda(predicate) = &c.args[2].expr else {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2631,
                    "quantifier predicate must be an expression-bodied |index:i64| binder",
                )
                .with_label(Label::primary(c.args[2].span, "invalid predicate")),
            );
            return Self::error_type();
        };
        if predicate.params.len() != 1
            || predicate.params[0]
                .ty
                .as_ref()
                .is_some_and(|ty| *ty != Type::I64)
            || predicate
                .return_type
                .as_ref()
                .is_some_and(|ty| *ty != Type::Bool)
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2631,
                    "quantifier predicate requires one i64 binder and a bool result",
                )
                .with_label(Label::primary(
                    predicate.span,
                    "invalid predicate signature",
                )),
            );
            return Self::error_type();
        }
        let LambdaBody::Expr(body) = &predicate.body else {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2631,
                    "quantifier predicates must be expression-bodied",
                )
                .with_label(Label::primary(predicate.span, "unsupported predicate body")),
            );
            return Self::error_type();
        };
        self.symbols.push_scope();
        self.define_var(
            predicate.params[0].name.clone(),
            VarInfo {
                ty: Type::I64,
                mutable: false,
                is_param: false,
                declaration_span: predicate.params[0].span,
            },
        );
        let ty = self.check_expr(body);
        self.symbols.pop_scope();
        if ty != Type::Bool {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2631,
                    "quantifier predicate must have type bool",
                )
                .with_label(Label::primary(body.span(), "invalid predicate result")),
            );
        }
        Type::Bool
    }

    fn check_named_call(&mut self, c: &CallExpr) -> Type {
        // Lexical value bindings win over module/free-function names.
        // Looking up the free function first silently compiled
        // `let f = |...| ...; f(...)` as a direct call when a top-level
        // `fn f` also existed.
        if let Some(var_info) = self.symbols.lookup_var(&c.callee).cloned() {
            if !c.type_args.is_empty() {
                self.generic_error(
                    c.span,
                    format!(
                        "function value `{}` does not accept type arguments",
                        c.callee
                    ),
                );
            }
            match &var_info.ty {
                // Both callable types are called the same way in source; they differ only in what codegen
                //
                // has to load first.
                Type::Fn(param_types, ret) | Type::Closure(param_types, ret) => {
                    self.local
                        .capture_flow
                        .calls
                        .insert(c.span, var_info.declaration_span);
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
                    self.check_value_call_args(param_types, &c.args);
                    return ret.as_ref().clone();
                }
                ty => {
                    for arg in &c.args {
                        self.check_expr(&arg.expr);
                    }
                    if Self::contains_error_type(ty) {
                        return Self::error_type();
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "cannot call value `{}` of type `{}`",
                                c.callee,
                                self.diagnostic_type_name(ty)
                            ),
                        )
                        .with_label(Label::primary(c.span, "this binding is not a function")),
                    );
                    return Type::Void;
                }
            }
        }

        if let Some(ty) = self.check_generic_call(c, CallContext::default()) {
            return ty;
        }
        if !c.type_args.is_empty() {
            self.generic_error(
                c.span,
                format!("function `{}` does not accept type arguments", c.callee),
            );
        }
        if c.callee == "format" {
            return self.check_format_call(c);
        }
        // Variadic formatted panic: `panic(spec, args...)`.
        // The one-argument form stays a plain-String builtin call.
        if c.callee == "panic" && c.args.len() != 1 {
            return self.check_panic_interpolation(c);
        }

        // Direct call to a named function.
        if let Some(info) = self.symbols.lookup_func(&c.callee).cloned() {
            if info.constant.is_some() {
                return self.reject_constant_call(&c.callee, &info, &c.args, c.span);
            }
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
            if info.foreign {
                self.check_foreign_call(c.span, info.is_async);
            }
            self.check_call_args_against_param_infos(&info.param_infos, &c.args, false);
            self.check_async_call_captures(&info.param_infos, &c.args, info.is_async);
            self.record_sending_call(c.id, &info, &c.args);
            if let Some(operation) = self.imported_blocking_std_functions.get(&c.callee).copied() {
                self.record_direct_lock_effect(c.span, operation, LockEffectKind::Block);
            }
            self.record_lock_effect_call(FunctionId::free_from_source_name(&c.callee), c.span);
            return function_call_return_type(&info);
        }

        // Prelude constructors can infer their payload without an outer
        // annotation, just like the qualified Result::Ok/Err forms.
        let inferred_enum = match c.callee.as_str() {
            "Ok" | "Err" => Some(B::Result.source_name()),
            _ => None,
        };
        if let Some(name) = inferred_enum
            && self.prelude_variant_name_is_unshadowed(&c.callee)
            && self
                .symbols
                .lookup_enum(name)
                .is_some_and(|info| info.name == name)
        {
            let ty = self.resolve_static_call(
                name,
                &[],
                &c.callee,
                &c.args,
                (c.id, c.span, c.span),
                CallContext::default(),
            );
            self.enum_variant_resolutions.insert(c.id, name.to_string());
            return ty;
        }

        if self.report_contextless_variant(&c.callee, c.span) {
            for arg in &c.args {
                self.check_expr(&arg.expr);
            }
            return Self::error_type();
        }
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0350,
            format!("cannot find function `{}`", c.callee),
        )
        .with_label(Label::primary(c.span, "not found in this scope"));
        if let Some(name) = self.similar_function_name(&c.callee) {
            diagnostic = diagnostic.with_help(format!("did you mean `{name}`?"));
        }
        self.push(diagnostic);
        Type::Void
    }

    /// A visible free function whose name is a likely typo of `callee`, or
    /// the leading word of it (`yield_now` -> `yield`).
    fn similar_function_name(&self, callee: &str) -> Option<String> {
        let names: Vec<_> = self
            .symbols
            .functions
            .keys()
            .filter(|id| id.namespace().is_none() && id.owner().is_none() && !id.is_lambda())
            .map(|id| id.name())
            // Print functions are keywords, not entries in the function table.
            .chain(["print", "println"].map(std::sync::Arc::<str>::from))
            .filter(|name| {
                name.as_ref() != callee
                    && name.starts_with(|c: char| c.is_ascii_alphabetic())
                    && name.len().abs_diff(callee.len()) <= 2
                    || callee.split('_').next() == Some(name.as_ref())
            })
            .collect();
        if let Some(name) = super::diagnostics::suggest_similar_name(callee, names.iter()) {
            return Some(name);
        }
        let head = callee
            .split('_')
            .next()
            .filter(|head| head.len() >= 3 && *head != callee)?;
        names
            .iter()
            .find(|name| name.as_ref() == head)
            .map(|name| name.to_string())
    }

    fn check_await_expr(&mut self, a: &AwaitExpr) -> Type {
        self.record_direct_lock_effect(a.span, "await", LockEffectKind::Suspend);
        let errors_before = self.error_generation;
        let awaited_ty = self.check_expr(&a.expr);
        if !self.local.current_async_context {
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
        if Self::contains_error_type(&awaited_ty) || self.error_generation != errors_before {
            return Self::error_type();
        }
        // `Task<T>`/`JoinHandle<T>` yield `T`; `Future<T>` yields `T`;
        // `await task.result()` is the same wait, same
        // frame and same waiter registration as `await task` — only the
        // cancelled mapping differs, so `TaskResult<T>` yields
        // `Result<T, Cancelled>`. `JoinHandle<T>` must be here because
        // it is what E0812 tells users to migrate `join()` to.
        match await_output_type(&awaited_ty) {
            Some(ty) => ty,
            None => {
                let other = awaited_ty;
                if let Some(replacement) = self.sync_fs_await_replacement(&a.expr) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0803,
                            "synchronous filesystem operations cannot be awaited",
                        )
                        .with_label(Label::primary(
                            a.expr.span(),
                            format!(
                                "this call returns `{}` immediately",
                                self.diagnostic_type_name(&other)
                            ),
                        ))
                        .with_help(format!(
                            "use `await {replacement}(...)` to run on the blocking pool, or remove `await` to keep the synchronous call"
                        )),
                    );
                    return Type::Void;
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0803,
                        format!("cannot await value of type `{}`", self.diagnostic_type_name(&other)),
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

    pub(super) fn check_print_expr(&mut self, arg: &Expr, newline: &bool) -> Type {
        let arg_ty = self.check_expr(arg);
        // Printable: i64/f64/bool/String and Never (a panicking
        // argument never reaches the print). Option values require
        // explicit matching/unwrapping. Anything else used to compile
        // and silently print NOTHING.
        if !matches!(
            &arg_ty,
            Type::I8
                | Type::I16
                | Type::I32
                | Type::U16
                | Type::U32
                | Type::U64
                | Type::Char
                | Type::U8
                | Type::I64
                | Type::F64
                | Type::Bool
                | Type::String
                | Type::Never
        ) && !Self::contains_error_type(&arg_ty)
        {
            let fn_name = if *newline { "println" } else { "print" };
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1402,
                    format!(
                        "cannot {fn_name} a value of type `{}`",
                        self.diagnostic_type_name(&arg_ty)
                    ),
                )
                .with_label(Label::primary(
                    arg.span(),
                    "printable types are i64, f64, bool, and String",
                ))
                .with_help(
                    "convert the value first, e.g. with `match`, `.to_string()`, or `format`",
                ),
            );
        }
        Type::Void
    }

    fn check_ternary_condition(&mut self, condition: &Expr, checked: bool) {
        let part = super::body::FailedCheck::Condition(condition.id());
        if checked {
            self.reuse_check_failure(part);
            return;
        }
        let errors_before = self.error_generation;
        let cond_ty = self.check_expr(condition);
        if cond_ty != Type::Bool
            && self.error_generation == errors_before
            && !Self::contains_error_type(&cond_ty)
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0901,
                    format!(
                        "if-expression condition must be `bool`, found `{}`",
                        self.diagnostic_type_name(&cond_ty)
                    ),
                )
                .with_label(Label::primary(
                    condition.span(),
                    format!(
                        "expected `bool`, found `{}`",
                        self.diagnostic_type_name(&cond_ty)
                    ),
                )),
            );
        }
        if self.error_generation != 0 {
            self.record_check_failure(part, self.error_generation != errors_before);
        }
    }

    pub(super) fn check_ternary_expr(
        &mut self,
        t: &TernaryExpr,
        expected: Option<&Type>,
        checked: bool,
    ) -> Type {
        self.check_ternary_condition(&t.condition, checked);
        let then_ty = match expected {
            Some(ty) => self.check_argument_expecting(&t.then_expr, ty, checked),
            None => self.check_branch_value(&t.then_expr),
        };
        let else_ty = match expected {
            Some(ty) => self.check_argument_expecting(&t.else_expr, ty, checked),
            None => self.check_branch_value(&t.else_expr),
        };
        let unified = if expected.is_some_and(crate::semantic::generic_inference::has_enum_hole) {
            match (&then_ty, &else_ty) {
                (Type::Never, _) => Some(else_ty.clone()),
                (_, Type::Never) => Some(then_ty.clone()),
                _ => crate::semantic::generic_inference::merge_enum_holes(&then_ty, &else_ty),
            }
        } else {
            self.unify_ternary_types(&then_ty, &else_ty)
                .or_else(|| self.unify_to_expected(expected, &then_ty, &else_ty))
        };
        if let Some(unified_ty) = unified {
            if expected.is_none() {
                self.resolve_generic_values([&t.then_expr, &t.else_expr], &unified_ty);
            }
            self.validate_type(&unified_ty, t.span);
            unified_ty
        } else {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E0902,
                format!(
                    "ternary branches have incompatible types: `{}` and `{}`",
                    self.diagnostic_type_name(&then_ty),
                    self.diagnostic_type_name(&else_ty)
                ),
            )
            .with_label(Label::primary(
                t.else_expr.span(),
                format!(
                    "expected `{}`, found `{}`",
                    self.diagnostic_type_name(&then_ty),
                    self.diagnostic_type_name(&else_ty)
                ),
            ))
            .with_label(Label::secondary(
                t.then_expr.span(),
                format!(
                    "this branch has type `{}`",
                    self.diagnostic_type_name(&then_ty)
                ),
            ));
            if expected.is_none() {
                diagnostic = diagnostic.with_help(self.common_supertype_help(&then_ty, &else_ty));
            }
            self.push(diagnostic);
            Type::Void
        }
    }

    /// Record direct built-in waits using the receiver's checked type. This is
    /// shared by the transitive helper summary and the direct lock-body scan:
    /// a user class that happens to name a method `send` must stay effect-free.
    fn record_builtin_lock_effect(&mut self, intrinsic: Option<Intrinsic>, call: &MethodCallExpr) {
        let effect = intrinsic.and_then(|id| match id {
            Intrinsic::CellGet => Some(("BlockingCell.get", LockEffectKind::Block)),
            Intrinsic::CellSet => Some(("BlockingCell.set", LockEffectKind::Block)),
            Intrinsic::RwCellRead => Some(("BlockingRwCell.read", LockEffectKind::Block)),
            Intrinsic::RwCellWrite => Some(("BlockingRwCell.write", LockEffectKind::Block)),
            _ => id
                .channel_wait_name()
                .map(|name| (name, LockEffectKind::Suspend)),
        });
        if let Some((operation, kind)) = effect {
            self.record_direct_lock_effect(call.span, operation, kind);
        }
    }

    /// Synchronous compatibility file APIs execute on the current native
    /// worker. Their `_async` counterparts only create a Task here and suspend
    /// later at `await`, so they are intentionally absent.
    fn record_static_builtin_lock_effect(&mut self, call: &StaticCallExpr) {
        let is_fs = call.class == "fs"
            || call.class == "std::fs"
            || self
                .imported_std_modules
                .get(&call.class)
                .is_some_and(|module| module.module == "fs");
        if !is_fs {
            return;
        }
        let operation = match call.method.as_str() {
            "read_to_string" => Some("fs::read_to_string"),
            "write_string" => Some("fs::write_string"),
            "exists" => Some("fs::exists"),
            "remove_file" => Some("fs::remove_file"),
            _ => None,
        };
        if let Some(operation) = operation {
            self.record_direct_lock_effect(call.span, operation, LockEffectKind::Block);
        }
    }

    /// Migration target when `await` is applied to an unsuffixed synchronous
    /// std::fs call. The explicit suffix keeps sync compatibility and makes the
    /// scheduling effect visible at the call site.
    fn sync_fs_await_replacement(&self, expr: &Expr) -> Option<String> {
        let sync_method = |method: &str| {
            matches!(
                method,
                "read_to_string" | "write_string" | "exists" | "remove_file"
            )
        };
        match expr {
            Expr::StaticCall(call) => {
                let is_fs = call.class == "fs"
                    || call.class == "std::fs"
                    || self
                        .imported_std_modules
                        .get(&call.class)
                        .is_some_and(|module| module.module == "fs");
                (is_fs && sync_method(&call.method)).then(|| {
                    QualifiedName::join(&call.class, format_args!("{}_async", call.method))
                })
            }
            Expr::Call(call) => self
                .imported_blocking_std_functions
                .get(&call.callee)
                .and_then(|operation| {
                    QualifiedName::new(operation)
                        .split_last()
                        .map(|(_, method)| method)
                })
                .filter(|method| sync_method(method))
                .map(|method| QualifiedName::join("fs", format_args!("{method}_async"))),
            _ => None,
        }
    }

    pub(super) fn check_range(&mut self, range: &RangeExpr) -> Type {
        // A range is a first-class `Range<i64>` value. Its bounds are checked
        // normally; a nested range bound surfaces as a non-`i64` bound below.
        let start_ty = self.check_expr(&range.start);
        let end_ty = self.check_expr(&range.end);

        // Judge each bound independently: an unresolved bound cannot justify
        // a mismatch, but must not hide a real error on the opposite bound.
        if (start_ty != Type::I64 && !Self::contains_error_type(&start_ty))
            || (end_ty != Type::I64 && !Self::contains_error_type(&end_ty))
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "range bounds must be `i64`, found `{}` and `{}`",
                        self.diagnostic_type_name(&start_ty),
                        self.diagnostic_type_name(&end_ty)
                    ),
                )
                .with_label(Label::primary(range.span, "range bounds must be `i64`")),
            );
        }

        range_type()
    }

    pub(super) fn check_method_call_expecting(
        &mut self,
        m: &MethodCallExpr,
        expected_receiver: Option<&Type>,
        checked: bool,
    ) -> Type {
        // `.` is instance member access; module items use `::`. Using
        // `math.add(..)` on a module is an error that points at `::`.
        if let Expr::Var(name, _, _) = &m.object
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
        if let Expr::Var(_, super_span, _) = &m.object
            && self.is_super_receiver(&m.object)
        {
            return self.check_super_method_call(m, *super_span);
        }
        let obj_ty = match expected_receiver {
            Some(expected) => self.check_argument_expecting(&m.object, expected, checked),
            // A method can resolve an omitted enum parameter (for example
            // `Result::Ok(1).and_then(f)`). Nested argument expressions still
            // use the ordinary inference boundary.
            None => self.check_branch_value(&m.object),
        };
        if Self::contains_error_type(&obj_ty) {
            for arg in &m.args {
                self.check_expr(&arg.expr);
            }
            return Self::error_type();
        }
        let references = self.analysis_symbols.references.len();
        let errors = self.error_generation;
        let result = self.check_receiver_method_call(&obj_ty, m);
        self.record_builtin_method_use(m, references, errors, &result);
        result
    }

    /// A type parameter has no bounds. Name an interface that declares the
    /// method, which the parameter's values could be taken as, and the
    /// function parameter that would carry the operation instead.
    fn explain_parameter_method(
        &self,
        diagnostic: Diagnostic,
        parameter: &str,
        method: &str,
    ) -> Diagnostic {
        // The alphabetically first owner keeps the suggestion deterministic.
        let owner = self
            .symbols
            .interfaces
            .values()
            .filter(|info| info.type_params.is_empty())
            .filter_map(|info| {
                let found = info.methods.get(method).filter(|m| !m.is_static)?;
                Some((info.name.as_str(), found))
            })
            .min_by_key(|(name, _)| *name);
        let diagnostic = diagnostic.with_note(format!(
            "a type parameter accepts every type and Willow has no interface bounds, so `{parameter}` only supports operations valid for all types"
        ));
        match owner {
            Some((interface, info)) => {
                let params = info
                    .params
                    .iter()
                    .map(|ty| self.diagnostic_type_name(ty))
                    .collect::<Vec<_>>();
                let params = std::iter::once(parameter.to_string())
                    .chain(params)
                    .collect::<Vec<_>>()
                    .join(", ");
                let result = self.diagnostic_type_name(&info.return_type);
                diagnostic.with_help(format!(
                    "take `{interface}` instead of `{parameter}`, as in `x: {interface}` or `Array<{interface}>`, so `{method}` dispatches through the interface; or keep `{parameter}` and pass the operation as a parameter, as in `{method}: fn({params}) -> {result}`"
                ))
            }
            None => diagnostic.with_help(format!(
                "pass the operation as a function parameter, as in `{method}: fn({parameter}) -> R` with `R` its result type, or take an interface that declares `{method}` instead of `{parameter}`"
            )),
        }
    }

    /// Member calls on an already-checked, non-error receiver.
    pub(super) fn check_receiver_method_call(&mut self, obj_ty: &Type, m: &MethodCallExpr) -> Type {
        if let Type::Named(owner) = obj_ty
            && let Some((name, info)) = self.native_member(owner, &m.method, true)
        {
            return self.check_native_member(m.id, m.span, name, info, &m.args, true);
        }
        if let Type::Named(name) = obj_ty
            && self.generics.parameter_names.contains(name)
        {
            if self.generics.rejected_parameter_methods.insert(m.span) {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("type parameter `{name}` has no method `{}`", m.method),
                )
                .with_label(Label::primary(m.span, "unknown method on a type parameter"));
                self.label_receiver_call_origin(&mut diagnostic, &m.object, obj_ty);
                let diagnostic = self.explain_parameter_method(diagnostic, name, m.method.as_str());
                self.push(diagnostic);
            }
            for arg in &m.args {
                self.check_expr(&arg.expr);
            }
            return Self::error_type();
        }
        if self.generics.rejected_parameter_methods.contains(&m.span) {
            for arg in &m.args {
                self.check_expr(&arg.expr);
            }
            return Self::error_type();
        }
        let spelling = m.method.as_str();
        let name = m.method.kind();
        let (name, spelling, lookup) = if let Some((current, renamed)) = name.replacement()
            && !self.type_declares_method(obj_ty, spelling)
        {
            let replacement = renamed.lookup(obj_ty);
            if replacement.identity().is_some() || self.type_declares_method(obj_ty, current) {
                self.report_renamed_method(obj_ty, m, current);
                (renamed, current, replacement)
            } else {
                // Retired spellings have no builtin signature.
                (name, spelling, MethodLookup::Missing(name))
            }
        } else {
            (name, spelling, name.lookup(obj_ty))
        };
        if let Some((id, arity)) = lookup.identity() {
            self.builtin_calls.record(m.id, id, arity, m.args.len());
        }
        if let MethodLookup::Adt(method) = lookup {
            return self.check_option_result_method_call(obj_ty, m, spelling, method);
        }
        let intrinsic = match &lookup {
            MethodLookup::Intrinsic(sig) => Some(sig.intrinsic),
            _ => None,
        };
        self.record_builtin_lock_effect(intrinsic, m);
        let family = builtin_types::resolve(obj_ty).map(|ty| (ty.id, ty.args));
        match family {
            Some((kind @ (B::AtomicI64 | B::AtomicBool), _)) => {
                return self.check_atomic_method_call(
                    kind.source_name(),
                    m,
                    spelling,
                    lookup.into_signature(),
                );
            }
            Some((kind @ (B::CancellationToken | B::TaskScope), _)) => {
                return self.check_task_control_method_call(
                    kind.source_name(),
                    m,
                    spelling,
                    lookup,
                );
            }
            Some((B::Mutex | B::RwLock | B::BlockingCell | B::BlockingRwCell, [elem])) => {
                let Type::Generic(lock, _) = obj_ty else {
                    unreachable!()
                };
                return self.check_lock_method_call(
                    lock,
                    elem,
                    m,
                    spelling,
                    lookup.into_signature(),
                );
            }
            _ => {}
        }
        match lookup {
            MethodLookup::Intrinsic(sig)
                if matches!(
                    sig.intrinsic,
                    Intrinsic::TaskResult | Intrinsic::TaskCancel | Intrinsic::TaskIsCancelled
                ) =>
            {
                self.check_builtin_arity(
                    BuiltinArity::new(sig.arity, ArityText::Plain),
                    "Task",
                    spelling,
                    m.args.len(),
                    m.span,
                );
                return sig.return_type(|_| None);
            }
            MethodLookup::Intrinsic(sig) if sig.intrinsic == Intrinsic::ChannelSend => {
                return self.check_channel_send(obj_ty, m, spelling, sig.arity);
            }
            MethodLookup::Intrinsic(sig)
                if matches!(
                    sig.intrinsic,
                    Intrinsic::ChannelRecv | Intrinsic::ChannelRecvOpt | Intrinsic::ChannelClose
                ) =>
            {
                self.check_builtin_arity(
                    BuiltinArity::new(sig.arity, ArityText::Plain),
                    "Channel",
                    spelling,
                    m.args.len(),
                    m.span,
                );
                return sig.return_type(|_| None);
            }
            MethodLookup::Missing(wait)
                if wait.is_retired_wait() && is_task_handle_type(obj_ty) =>
            {
                return self.check_retired_task_wait(obj_ty, m, spelling, wait);
            }
            MethodLookup::Missing(name) => {
                if let Some(attempted) = name.channel()
                    && !matches!(obj_ty, Type::Named(class) if self.symbols.lookup_class(class).is_some())
                {
                    return self.check_invalid_channel_receiver(obj_ty, m, spelling, attempted);
                }
            }
            _ => {}
        }
        if let Type::Array(elem) | Type::FixedArray(elem, _) = obj_ty {
            return self.check_array_method_call(obj_ty, elem, m, spelling, lookup);
        }
        match family {
            Some((B::FrozenArray, [elem])) => {
                return self.check_frozen_array_method_call(elem, m, spelling, lookup);
            }
            Some((B::Map, [key, value])) => {
                return self.check_map_method_call(obj_ty, (key, value), m, spelling, lookup);
            }
            Some((B::FrozenMap, [key, value])) => {
                return self.check_frozen_map_method_call((key, value), m, spelling, lookup);
            }
            _ => {}
        }
        self.check_task_method_call(obj_ty, m, spelling);
        for callee in self.method_effect_ids(obj_ty, spelling) {
            self.record_lock_effect_call(callee, m.span);
        }
        self.resolve_method(
            obj_ty,
            m,
            spelling,
            true,
            Some((name, lookup.into_signature())),
        )
    }

    fn report_renamed_method(&mut self, obj_ty: &Type, m: &MethodCallExpr, current: &str) {
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0201,
            format!(
                "no method `{}` on type `{}`; it was renamed to `{current}`",
                m.method,
                self.diagnostic_type_name(obj_ty)
            ),
        )
        .with_label(Label::primary(m.span, "old method name"))
        .with_help(format!(
            "builtin and std method names are snake_case: write `.{current}(...)`"
        ));
        self.label_receiver_call_origin(&mut diagnostic, &m.object, obj_ty);
        self.push(diagnostic);
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

/// Walk executable syntax in a deferred body. Lambda bodies are deliberately
/// opaque: they are separate functions, so their control flow cannot escape
/// the defer body that merely constructs the function value. Nested `defer`
/// bodies validate themselves when `check_stmt` reaches them.
fn walk_defer_body(
    body: &DeferBody,
    on_stmt: &mut impl FnMut(&Stmt),
    on_expr: &mut impl FnMut(&Expr),
) {
    use crate::parser::iter::{AstEvent, AstWalk};
    let root = match body {
        DeferBody::Expr(expr) => AstEvent::Expr(expr),
        DeferBody::Block(block) => AstEvent::Block(block),
    };
    let mut walk = AstWalk::new(root);
    while let Some(event) = walk.next() {
        match event {
            AstEvent::Stmt(stmt) => {
                on_stmt(stmt);
                if matches!(stmt, Stmt::Defer(_)) {
                    walk.skip_children();
                }
            }
            AstEvent::Expr(expr) => {
                on_expr(expr);
                if matches!(expr, Expr::Lambda(_)) {
                    walk.skip_children();
                }
            }
            _ => {}
        }
    }
}

fn defer_control_flow_violations(body: &DeferBody) -> Vec<(Span, &'static str)> {
    let mut stmt_violations = Vec::new();
    let mut expr_violations = Vec::new();
    walk_defer_body(
        body,
        &mut |stmt| match stmt {
            Stmt::Return(stmt) => stmt_violations.push((stmt.span, "return")),
            Stmt::Break(span) => stmt_violations.push((*span, "break")),
            Stmt::Continue(span) => stmt_violations.push((*span, "continue")),
            _ => {}
        },
        &mut |expr| {
            if let Expr::TryPropagate(_, span, _) = expr {
                expr_violations.push((*span, "?"));
            }
        },
    );
    stmt_violations.extend(expr_violations);
    stmt_violations
}

fn defer_body_contains_suspend(body: &DeferBody) -> bool {
    let mut contains = false;
    walk_defer_body(body, &mut |_| {}, &mut |expr| {
        contains |= matches!(expr, Expr::Await(_) | Expr::Select(_));
    });
    contains
}

/// Whether this deferred AST directly contains the compiler-known
/// `recover()` builtin. The generic defer walker deliberately treats lambdas
/// as opaque, so a helper/lambda cannot inherit the caller's recovery
/// capability.
pub(crate) fn defer_body_contains_direct_recover(body: &DeferBody) -> bool {
    let mut contains = false;
    walk_defer_body(body, &mut |_| {}, &mut |expr| {
        contains |= matches!(expr, Expr::Call(call) if call.callee == "recover");
    });
    contains
}

const DEFER_ASYNC_HELP: &str =
    "perform the async cleanup explicitly, or defer a synchronous helper";

fn defer_async_call_span(body: &DeferBody, expr_types: &HashMap<ExprId, Type>) -> Option<Span> {
    let mut found = None;
    // An awaited call is already reported by the suspension error; the walk
    // visits each `await` before its operand.
    let mut awaited: Option<ExprId> = None;
    walk_defer_body(body, &mut |_| {}, &mut |expr| {
        if let Expr::Await(a) = expr {
            awaited = Some(a.expr.id());
            return;
        }
        if awaited.take() == Some(expr.id()) {
            return;
        }
        if found.is_some()
            || !matches!(
                expr,
                Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
            )
        {
            return;
        }
        if expr_types.get(&expr.id()).is_some_and(|ty| {
            builtin_types::resolve(ty)
                .is_some_and(|resolved| matches!(resolved.id, B::Task | B::Future))
        }) {
            found = Some(expr.span());
        }
    });
    found
}

fn defer_scheduler_drive_span(
    body: &DeferBody,
    builtin_calls: &crate::semantic::intrinsics::BuiltinCalls,
) -> Option<(Span, &'static str)> {
    let mut statement = None;
    let mut expression = None;
    walk_defer_body(
        body,
        &mut |stmt| {
            if statement.is_none()
                && let Stmt::Lock(lock) = stmt
            {
                statement = Some((lock.span, "lock"));
            }
        },
        &mut |expr| {
            if expression.is_some() {
                return;
            }
            let Expr::MethodCall(call) = expr else {
                return;
            };
            if let Some(name) = builtin_calls
                .candidate(&call.id)
                .and_then(|id| id.channel_wait_name())
            {
                expression = Some((call.span, name));
            }
        },
    );
    statement.or(expression)
}

/// Perspectives for the checker's virtual-dispatch resolution in the E2604
/// lock-effect graph.
///
/// Before this, `concrete_method_effect_id` resolved a receiver call by
/// walking *up* from the receiver's declared class to the nearest declaring
/// body. That answers "which body does an instance of exactly this class
/// run", which is not the question a call site asks: the declared type only
/// bounds the dynamic type from *above*, so a subclass override was invisible
/// and a waiting override under a lock compiled clean. Resolution now goes
/// through the shared [`ClassHierarchy::dispatch_targets`], the same union the
/// backend's panic-effect analysis uses — the virtual-dispatch effect analysis defect class.
///
/// Perspectives:
///   d01 direct  — the receiver's own class declares the waiting body
///   d02 union   — a subclass override waits, the base body is pure
///   d03 pure    — base and override both pure, no diagnostic
///   d04 self    — `self.hook()` in a base body reaches a subclass override
///   d05 deep    — three-level chain, the leaf overrides and waits
///   d06 sibling — one of two siblings waits; base receiver sees it
///   d07 narrow  — the pure sibling's own type does not see it
///   d08 inherit — a subclass receiver that does not override reaches the base
///   d09 static  — a static method of the same name is not a dispatch target
///   d10 staticcall — `Self::helper()` still resolves as a direct edge
///   d11 ctor    — `new C()` reaches a waiting `init`
///   d12 iface   — an interface receiver still resolves to its union node
///   d13 once    — N waiting overrides produce exactly one diagnostic
///   d14 unrelated — a same-named method in a disjoint hierarchy is excluded
///   d15 generic — a `Type::Generic` receiver resolves like its named base
///   d16 recur   — a self-recursive waiting method terminates and reports
///   d17 async   — an async override does not propagate into a sync caller
///   d18 sealed  — a non-open class has itself as its only dispatch target
///   d19 outside — the same call outside the lock is legal
///   d20 nested  — the override's wait is reached through an extra helper hop
///   d21 twoargs — the union is keyed by method name, not by signature shape
///   d22 order   — the reported witness is stable regardless of declaration order
///   d23 severity — a caller reaching both kinds of wait reports the blocking one
#[cfg(test)]
mod lock_effect_dispatch_tests;

#[cfg(test)]
mod condition_recovery_tests;

#[cfg(test)]
mod source_array_constructor_tests;
